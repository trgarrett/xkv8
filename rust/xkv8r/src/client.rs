//! RPC client construction and push-tx helpers.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chia_protocol::SpendBundle;
use chia_wallet_sdk::coinset::{
    ChiaRpcClient, CoinsetClient, FullNodeClient, PushTxResponse,
};
use native_tls::{Identity, TlsConnector};

use crate::config::Config;

// ── Raw-body push_tx helper ────────────────────────────────────────────

/// Maximum bytes of a bad response body to include in the error message.
const RAW_BODY_SNIPPET_LEN: usize = 512;

/// POST `spend_bundle` to `url` using `client`, read the raw response bytes,
/// then parse them as JSON.  On any failure the raw body (up to
/// `RAW_BODY_SNIPPET_LEN` bytes, lossily decoded as UTF-8) is embedded in the
/// returned error so the caller can see exactly what the server sent back.
/// Recursively prefix every bare hex string value in a JSON tree with `"0x"`.
///
/// The `chia-protocol` serde impl emits plain hex (e.g. `"abc123…"`) but the
/// Chia Python RPC requires `"0x"`-prefixed hex on every bytes field.
pub(crate) fn prefix_hex_values(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            if !s.starts_with("0x")
                && !s.is_empty()
                && s.chars().all(|c| c.is_ascii_hexdigit())
            {
                *s = format!("0x{s}");
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                prefix_hex_values(item);
            }
        }
        serde_json::Value::Object(map) => {
            for val in map.values_mut() {
                prefix_hex_values(val);
            }
        }
        _ => {}
    }
}

async fn push_tx_raw(
    http: &reqwest::Client,
    url: &str,
    bundle: SpendBundle,
) -> Result<PushTxResponse> {
    // Serialise the bundle as a JSON object — the Chia RPC expects:
    //   {"spend_bundle": {"coin_spends": [...], "aggregated_signature": "..."}}
    // SpendBundle implements serde::Serialize and produces exactly this shape,
    // except the chia-protocol serde impl uses plain hex; the Python RPC needs
    // 0x-prefixed hex on every bytes field.
    let mut bundle_val = serde_json::to_value(&bundle)
        .with_context(|| "serialising SpendBundle to JSON")?;
    prefix_hex_values(&mut bundle_val);
    let body = serde_json::json!({ "spend_bundle": bundle_val });

    let response = http
        .post(url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let http_status = response.status();
    let raw = response
        .bytes()
        .await
        .with_context(|| format!("reading response body from {url}"))?;

    // Primary parse: the happy-path response includes `status`.
    if let Ok(parsed) = serde_json::from_slice::<PushTxResponse>(&raw) {
        return Ok(parsed);
    }

    // Fallback parse: the Chia full node and some API proxies return error
    // bodies that omit the `status` field (e.g. UNKNOWN_UNSPENT, rate-limit
    // 429s).  Deserialising those as a partial object lets us synthesise a
    // proper PushTxResponse so the caller can classify and retry correctly
    // instead of treating every such response as an opaque transport error.
    #[derive(serde::Deserialize)]
    struct PartialPushTx {
        success: Option<bool>,
        error: Option<String>,
        // coinset.org rate-limit bodies use `limit_reason` instead of `error`
        limit_reason: Option<String>,
        limit_message: Option<String>,
    }

    if let Ok(partial) = serde_json::from_slice::<PartialPushTx>(&raw) {
        let success = partial.success.unwrap_or(false);
        // Prefer `error`, fall back to `limit_reason` / `limit_message`
        let error = partial
            .error
            .or_else(|| partial.limit_reason.clone())
            .or(partial.limit_message);
        return Ok(PushTxResponse {
            status: if success {
                "SUCCESS".to_string()
            } else {
                "FAILED".to_string()
            },
            error,
            success,
        });
    }

    // Nothing worked — surface the raw body in the error message.
    let snippet = String::from_utf8_lossy(
        raw.get(..RAW_BODY_SNIPPET_LEN.min(raw.len())).unwrap_or(&raw),
    );
    Err(anyhow::anyhow!(
        "error decoding response body from {url} (HTTP {http_status}): unrecognised response shape\n  raw body: {snippet}"
    ))
}

// ── Abstract client wrapper ────────────────────────────────────────────

/// Trait-object wrapper so we can store both `CoinsetClient` and
/// `FullNodeClient` in the same `Vec`.
#[async_trait::async_trait]
pub trait RpcClient: Send + Sync + std::fmt::Debug {
    async fn get_blockchain_state(
        &self,
    ) -> Result<chia_wallet_sdk::coinset::BlockchainStateResponse>;

    async fn get_coin_records_by_puzzle_hash(
        &self,
        puzzle_hash: chia_protocol::Bytes32,
        start_height: Option<u32>,
        end_height: Option<u32>,
        include_spent: Option<bool>,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordsResponse>;

    async fn get_coin_record_by_name(
        &self,
        name: chia_protocol::Bytes32,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordResponse>;

    async fn get_puzzle_and_solution(
        &self,
        coin_id: chia_protocol::Bytes32,
        height: Option<u32>,
    ) -> Result<chia_wallet_sdk::coinset::GetPuzzleAndSolutionResponse>;

    async fn push_tx(&self, bundle: SpendBundle) -> Result<PushTxResponse>;
}

// ── CoinsetClient adapter ──────────────────────────────────────────────

/// Base URL used for all coinset endpoints (mainnet).
const COINSET_MAINNET_URL: &str = "https://api.coinset.org";
/// Base URL used for all coinset endpoints (testnet11).
const COINSET_TESTNET_URL: &str = "https://testnet11.api.coinset.org";

#[derive(Debug, Clone)]
pub struct CoinsetAdapter {
    inner: CoinsetClient,
    /// Plain HTTPS client used for the hardened `push_tx` that captures the raw body.
    http: reqwest::Client,
    push_url: String,
}

impl CoinsetAdapter {
    pub fn mainnet() -> Self {
        Self {
            inner: CoinsetClient::mainnet(),
            http: reqwest::Client::new(),
            push_url: format!("{COINSET_MAINNET_URL}/push_tx"),
        }
    }
    pub fn testnet11() -> Self {
        Self {
            inner: CoinsetClient::testnet11(),
            http: reqwest::Client::new(),
            push_url: format!("{COINSET_TESTNET_URL}/push_tx"),
        }
    }
}

#[async_trait::async_trait]
impl RpcClient for CoinsetAdapter {
    async fn get_blockchain_state(
        &self,
    ) -> Result<chia_wallet_sdk::coinset::BlockchainStateResponse> {
        Ok(ChiaRpcClient::get_blockchain_state(&self.inner).await.map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_coin_records_by_puzzle_hash(
        &self,
        puzzle_hash: chia_protocol::Bytes32,
        start_height: Option<u32>,
        end_height: Option<u32>,
        include_spent: Option<bool>,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordsResponse> {
        Ok(ChiaRpcClient::get_coin_records_by_puzzle_hash(
            &self.inner,
            puzzle_hash,
            start_height,
            end_height,
            include_spent,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_coin_record_by_name(
        &self,
        name: chia_protocol::Bytes32,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordResponse> {
        Ok(ChiaRpcClient::get_coin_record_by_name(&self.inner, name)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_puzzle_and_solution(
        &self,
        coin_id: chia_protocol::Bytes32,
        height: Option<u32>,
    ) -> Result<chia_wallet_sdk::coinset::GetPuzzleAndSolutionResponse> {
        Ok(ChiaRpcClient::get_puzzle_and_solution(&self.inner, coin_id, height)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    /// Hardened push: captures the raw response body on JSON-decode failure.
    async fn push_tx(&self, bundle: SpendBundle) -> Result<PushTxResponse> {
        push_tx_raw(&self.http, &self.push_url, bundle).await
    }
}

// ── FullNodeClient adapter ─────────────────────────────────────────────

#[derive(Debug)]
pub struct FullNodeAdapter {
    inner: FullNodeClient,
    /// mTLS client used for the hardened `push_tx` that captures the raw body.
    http: reqwest::Client,
    push_url: String,
}

impl FullNodeAdapter {
    pub fn new(cert_bytes: &[u8], key_bytes: &[u8]) -> Result<Self> {
        let inner = FullNodeClient::new(cert_bytes, key_bytes)
            .map_err(|e| anyhow::anyhow!("TLS client init: {e}"))?;
        let http = build_mtls_client(cert_bytes, key_bytes, "https://localhost:8555")?;
        Ok(Self {
            inner,
            http,
            push_url: "https://localhost:8555/push_tx".to_string(),
        })
    }
    pub fn with_url(url: String, cert_bytes: &[u8], key_bytes: &[u8]) -> Result<Self> {
        let inner = FullNodeClient::with_base_url(url.clone(), cert_bytes, key_bytes)
            .map_err(|e| anyhow::anyhow!("TLS client init: {e}"))?;
        let http = build_mtls_client(cert_bytes, key_bytes, &url)?;
        let push_url = format!("{}/push_tx", url.trim_end_matches('/'));
        Ok(Self { inner, http, push_url })
    }
}

/// Build a `reqwest::Client` that presents `cert_bytes`/`key_bytes` as a
/// client certificate and accepts any server certificate (the Chia full-node
/// uses a self-signed cert signed by the embedded Chia CA).
fn build_mtls_client(cert_bytes: &[u8], key_bytes: &[u8], _base_url: &str) -> Result<reqwest::Client> {
    // Combine PEM cert + key into a PKCS#12 Identity via native-tls.
    let identity = Identity::from_pkcs8(cert_bytes, key_bytes)
        .with_context(|| "building TLS identity for hardened push_tx client")?;

    let tls = TlsConnector::builder()
        .identity(identity)
        // The Chia node presents a self-signed cert; accept it.
        .danger_accept_invalid_certs(true)
        .build()
        .with_context(|| "building native-tls connector for hardened push_tx client")?;

    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .with_context(|| "building reqwest client for hardened push_tx")?;

    Ok(client)
}

#[async_trait::async_trait]
impl RpcClient for FullNodeAdapter {
    async fn get_blockchain_state(
        &self,
    ) -> Result<chia_wallet_sdk::coinset::BlockchainStateResponse> {
        Ok(ChiaRpcClient::get_blockchain_state(&self.inner).await.map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_coin_records_by_puzzle_hash(
        &self,
        puzzle_hash: chia_protocol::Bytes32,
        start_height: Option<u32>,
        end_height: Option<u32>,
        include_spent: Option<bool>,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordsResponse> {
        Ok(ChiaRpcClient::get_coin_records_by_puzzle_hash(
            &self.inner,
            puzzle_hash,
            start_height,
            end_height,
            include_spent,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_coin_record_by_name(
        &self,
        name: chia_protocol::Bytes32,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordResponse> {
        Ok(ChiaRpcClient::get_coin_record_by_name(&self.inner, name)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_puzzle_and_solution(
        &self,
        coin_id: chia_protocol::Bytes32,
        height: Option<u32>,
    ) -> Result<chia_wallet_sdk::coinset::GetPuzzleAndSolutionResponse> {
        Ok(ChiaRpcClient::get_puzzle_and_solution(&self.inner, coin_id, height)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    /// Hardened push: captures the raw response body on JSON-decode failure.
    async fn push_tx(&self, bundle: SpendBundle) -> Result<PushTxResponse> {
        push_tx_raw(&self.http, &self.push_url, bundle).await
    }
}

// ── Client builder ─────────────────────────────────────────────────────

/// Load the full-node TLS cert + key from `$CHIA_ROOT/config/ssl/full_node/`.
pub fn load_full_node_certs(chia_root: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let ssl_dir = chia_root.join("config").join("ssl").join("full_node");
    let cert_path = ssl_dir.join("private_full_node.crt");
    let key_path = ssl_dir.join("private_full_node.key");

    if !cert_path.exists() || !key_path.exists() {
        bail!(
            "Could not find full-node TLS certs in {}\n\
             Ensure your Chia node is set up, or set CHIA_ROOT to the correct directory.",
            ssl_dir.display()
        );
    }

    let cert = std::fs::read(&cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?;
    let key = std::fs::read(&key_path)
        .with_context(|| format!("reading {}", key_path.display()))?;
    Ok((cert, key))
}

/// Build the ordered list of RPC clients.
pub fn build_clients(config: &Config) -> Result<Vec<Arc<dyn RpcClient>>> {
    let mut clients: Vec<Arc<dyn RpcClient>> = Vec::new();

    if let Some(ref local) = config.local_full_node {
        let (cert, key) = load_full_node_certs(&config.chia_root)?;
        if local.contains(':') {
            let url = if local.starts_with("http") {
                local.clone()
            } else {
                format!("https://{local}")
            };
            println!("Using local full node RPC at {url} (native TLS)");
            clients.push(Arc::new(FullNodeAdapter::with_url(url, &cert, &key)?));
        } else {
            println!("Using local full node RPC at https://localhost:8555 (native TLS)");
            clients.push(Arc::new(FullNodeAdapter::new(&cert, &key)?));
        }
    }

    if config.is_testnet {
        clients.push(Arc::new(CoinsetAdapter::testnet11()));
    } else {
        clients.push(Arc::new(CoinsetAdapter::mainnet()));
    }

    Ok(clients)
}

// ── Push TX helpers ────────────────────────────────────────────────────

/// Structured result from pushing a transaction.
#[derive(Debug, Clone)]
pub struct PushTxResult {
    pub success: bool,
    pub status: Option<String>,
    pub error: Option<String>,
    pub error_category: &'static str,
    /// One entry per client: `(client_index, outcome_summary)`.
    /// Populated on failure so callers can log per-client diagnostics.
    pub per_client_errors: Vec<(usize, String)>,
}

fn classify_push_error(error: &str) -> &'static str {
    let upper = error.to_uppercase();
    if ["DOUBLE_SPEND", "ALREADY_INCLUDING", "CONFLICTING"]
        .iter()
        .any(|kw| upper.contains(kw))
    {
        return "mempool_conflict";
    }
    if ["COIN_NOT_YET", "UNKNOWN_UNSPENT"]
        .iter()
        .any(|kw| upper.contains(kw))
    {
        return "coin_not_ready";
    }
    if ["INVALID_FEE_TOO_CLOSE", "INVALID_FEE_LOW"]
        .iter()
        .any(|kw| upper.contains(kw))
    {
        return "fee_issue";
    }
    if ["TOO MANY REQUESTS", "RATE_LIMIT", "RATE LIMIT", "FULLNODE_RATE_LIMIT"]
        .iter()
        .any(|kw| upper.contains(kw))
    {
        return "rate_limit";
    }
    "unknown"
}

/// Push a spend bundle to all clients concurrently.
pub async fn push_tx_to_all(
    clients: &[Arc<dyn RpcClient>],
    bundle: &SpendBundle,
) -> PushTxResult {
    let futs: Vec<_> = clients
        .iter()
        .map(|c| {
            let c = Arc::clone(c);
            let b = bundle.clone();
            tokio::spawn(async move { c.push_tx(b).await })
        })
        .collect();

    let mut results = Vec::new();
    for fut in futs {
        results.push(fut.await);
    }

    // Prefer the first success
    for res in &results {
        if let Ok(Ok(tx_res)) = res {
            if tx_res.success {
                return PushTxResult {
                    success: true,
                    status: Some(tx_res.status.clone()),
                    error: None,
                    error_category: "success",
                    per_client_errors: Vec::new(),
                };
            }
        }
    }

    // Build per-client error summary for diagnostics
    let per_client_errors: Vec<(usize, String)> = results
        .iter()
        .enumerate()
        .map(|(i, res)| {
            let summary = match res {
                Ok(Ok(tx_res)) => format!(
                    "rpc_err: success={} status={:?} error={:?}",
                    tx_res.success, tx_res.status, tx_res.error
                ),
                Ok(Err(e)) => format!("http_err: {e}"),
                Err(e) => format!("join_err: {e}"),
            };
            (i, summary)
        })
        .collect();

    // No success — classify the primary (first) result
    match &results[0] {
        Ok(Ok(tx_res)) => {
            let error_str = tx_res.error.clone().unwrap_or_default();
            PushTxResult {
                success: false,
                status: Some(tx_res.status.clone()),
                error: Some(error_str.clone()),
                error_category: classify_push_error(&error_str),
                per_client_errors,
            }
        }
        Ok(Err(e)) => PushTxResult {
            success: false,
            status: None,
            error: Some(format!("{e}")),
            error_category: "transport",
            per_client_errors,
        },
        Err(e) => PushTxResult {
            success: false,
            status: None,
            error: Some(format!("{e}")),
            error_category: "transport",
            per_client_errors,
        },
    }
}

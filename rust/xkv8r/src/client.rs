//! RPC client construction and push-tx helpers.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chia_protocol::SpendBundle;
use chia_wallet_sdk::coinset::{
    ChiaRpcClient, CoinsetClient, FullNodeClient, PushTxResponse,
};

use crate::config::Config;

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

#[derive(Debug, Clone)]
pub struct CoinsetAdapter(CoinsetClient);

impl CoinsetAdapter {
    pub fn mainnet() -> Self {
        Self(CoinsetClient::mainnet())
    }
    pub fn testnet11() -> Self {
        Self(CoinsetClient::testnet11())
    }
}

#[async_trait::async_trait]
impl RpcClient for CoinsetAdapter {
    async fn get_blockchain_state(
        &self,
    ) -> Result<chia_wallet_sdk::coinset::BlockchainStateResponse> {
        Ok(ChiaRpcClient::get_blockchain_state(&self.0).await.map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_coin_records_by_puzzle_hash(
        &self,
        puzzle_hash: chia_protocol::Bytes32,
        start_height: Option<u32>,
        end_height: Option<u32>,
        include_spent: Option<bool>,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordsResponse> {
        Ok(ChiaRpcClient::get_coin_records_by_puzzle_hash(
            &self.0,
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
        Ok(ChiaRpcClient::get_coin_record_by_name(&self.0, name)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_puzzle_and_solution(
        &self,
        coin_id: chia_protocol::Bytes32,
        height: Option<u32>,
    ) -> Result<chia_wallet_sdk::coinset::GetPuzzleAndSolutionResponse> {
        Ok(ChiaRpcClient::get_puzzle_and_solution(&self.0, coin_id, height)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn push_tx(&self, bundle: SpendBundle) -> Result<PushTxResponse> {
        Ok(ChiaRpcClient::push_tx(&self.0, bundle)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
}

// ── FullNodeClient adapter ─────────────────────────────────────────────

#[derive(Debug)]
pub struct FullNodeAdapter(FullNodeClient);

impl FullNodeAdapter {
    pub fn new(cert_bytes: &[u8], key_bytes: &[u8]) -> Result<Self> {
        Ok(Self(
            FullNodeClient::new(cert_bytes, key_bytes)
                .map_err(|e| anyhow::anyhow!("TLS client init: {e}"))?,
        ))
    }
    pub fn with_url(url: String, cert_bytes: &[u8], key_bytes: &[u8]) -> Result<Self> {
        Ok(Self(
            FullNodeClient::with_base_url(url, cert_bytes, key_bytes)
                .map_err(|e| anyhow::anyhow!("TLS client init: {e}"))?,
        ))
    }
}

#[async_trait::async_trait]
impl RpcClient for FullNodeAdapter {
    async fn get_blockchain_state(
        &self,
    ) -> Result<chia_wallet_sdk::coinset::BlockchainStateResponse> {
        Ok(ChiaRpcClient::get_blockchain_state(&self.0).await.map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_coin_records_by_puzzle_hash(
        &self,
        puzzle_hash: chia_protocol::Bytes32,
        start_height: Option<u32>,
        end_height: Option<u32>,
        include_spent: Option<bool>,
    ) -> Result<chia_wallet_sdk::coinset::GetCoinRecordsResponse> {
        Ok(ChiaRpcClient::get_coin_records_by_puzzle_hash(
            &self.0,
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
        Ok(ChiaRpcClient::get_coin_record_by_name(&self.0, name)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn get_puzzle_and_solution(
        &self,
        coin_id: chia_protocol::Bytes32,
        height: Option<u32>,
    ) -> Result<chia_wallet_sdk::coinset::GetPuzzleAndSolutionResponse> {
        Ok(ChiaRpcClient::get_puzzle_and_solution(&self.0, coin_id, height)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
    }
    async fn push_tx(&self, bundle: SpendBundle) -> Result<PushTxResponse> {
        Ok(ChiaRpcClient::push_tx(&self.0, bundle)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?)
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
#[derive(Debug)]
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

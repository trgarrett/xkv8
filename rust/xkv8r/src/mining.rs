//! Main mining loops: polling-based and instant-react (Peer subscription).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chia_bls::{PublicKey, SecretKey};
use chia_protocol::{
    BlockRecord, Bytes32, Coin, CoinSpend, CoinStateFilters,
    NewPeakWallet, ProtocolMessageTypes, SpendBundle,
};
use chia_puzzle_types::DeriveSynthetic;
use chia_puzzle_types::standard::StandardArgs;
use chia_traits::Streamable;
use chia_ssl::ChiaCertificate;
use chia_wallet_sdk::client::{
    PeerOptions, connect_peer, create_rustls_connector,
};
use chia_wallet_sdk::driver::{Cat, Puzzle};
use chia_wallet_sdk::types::{Condition, run_puzzle};
use chia_wallet_sdk::utils::Address;
use clvm_traits::FromClvm;
use clvmr::Allocator;

use crate::bundle::build_mining_bundle;
use crate::client::{self, RpcClient, push_tx_to_all};
use crate::config::{Config, CAT_TAIL_HASH, GENESIS_HEIGHT};
use crate::pow::find_valid_nonce;
use crate::puzzle::{
    build_curried_puzzle_hash, full_cat_puzzlehash, get_difficulty_bits, get_epoch,
    get_reward,
};

const ERROR_SLEEP_SECS: f64 = 2.0;
const MAX_NONCE_ATTEMPTS: u64 = 5_000_000;

const EXCAVATOR_ART: &str = r#"
  .-.
 \   /
| (*) |-----....._____
''.  |--.._           '--.._
 | |  |     ''--.._       o  '.
 | |  |             ''--.._\  \
 | |  |                    \ \  \________
 | |  |                     \ \ /____  _ |
'-|__|                      \ //    || ||_________ .-----. _
 | /*)                       //_____||=||=================|
 |/-|                        \_________|_________________|
.'  \                        '----._______.-------------`
/     \                       ~.~.~.~.~.~.~.~.~.~.~.~.~.~
\      '._.                  ((*))o o ======= o o o (*) ))
 '.......`                   '-.~.~.~.~.~.~.~.~.~.~.~.~- `
"#;

/// Submitted coins tracker: coin_id → mine_height.
type SubmittedCoins = HashMap<Bytes32, u32>;

/// Main mining entry point.
pub async fn mine(config: Arc<Config>) -> Result<()> {
    let clients = client::build_clients(&config)?;

    // Build curried puzzle hash
    let inner_puzzle_hash = build_curried_puzzle_hash()?;
    let full_cat_ph = full_cat_puzzlehash(inner_puzzle_hash);

    println!("Lode puzzle hash: {}", hex::encode(inner_puzzle_hash));
    println!("Lode full CAT puzzle hash: {}", hex::encode(full_cat_ph));
    if config.thread_count > 1 {
        println!(
            "Mining with up to {} threads for nonce grinding",
            config.thread_count
        );
    }

    // Load miner key
    let sk = &config.miner_sk;
    let pk = sk.public_key();
    let pk_bytes = pk.to_bytes();
    println!("Miner public key: {}", hex::encode(&pk_bytes));
    println!("Mining to address: {}", config.target_address);

    // Derive synthetic key for fee spending
    let synthetic_sk: SecretKey = DeriveSynthetic::derive_synthetic(sk);
    let synthetic_pk = synthetic_sk.public_key();
    let fee_puzzlehash: Bytes32 = StandardArgs::curry_tree_hash(synthetic_pk).into();
    let fee_prefix = if config.is_testnet { "txch" } else { "xch" };
    let fee_address = Address::new(fee_puzzlehash, fee_prefix.to_string());
    let fee_address_str = fee_address.encode().unwrap_or_else(|_| "?".to_string());

    if config.fee_mojos > 0 {
        println!("Fee mode: {} mojos per spend", config.fee_mojos);
        println!("Fee address: {fee_address_str}");
        println!("  → Send XCH to this address to enable fee-boosted mining");
    } else {
        println!(
            "Fee address (not active, set FEE_MOJOS to enable): {fee_address_str}"
        );
    }
    println!();

    // Dispatch to instant-react or polling
    if config.local_full_node.is_some() {
        println!("⚡ Instant-react mining mode (Peer subscriptions)");
        let mut reorg_retries = 0u32;
        loop {
            match mine_instant_react(
                &clients,
                &config,
                inner_puzzle_hash,
                full_cat_ph,
                &pk_bytes,
                sk,
                fee_puzzlehash,
                &fee_address_str,
                &synthetic_sk,
                &synthetic_pk,
            )
            .await
            {
                Ok(()) => break,
                Err(e) => {
                    let err_str = format!("{e}");
                    if err_str.contains("onnect") || err_str.contains("losed") {
                        reorg_retries = 0;
                        eprintln!("Peer connection lost: {e}");
                        eprintln!("Reconnecting in 3 seconds…");
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    } else if err_str.contains("Reorg") {
                        reorg_retries += 1;
                        if reorg_retries >= 3 {
                            eprintln!(
                                "Peer rejected subscription due to chain reorg {reorg_retries} times — falling back to polling mode"
                            );
                            mine_polling(
                                &clients,
                                &config,
                                inner_puzzle_hash,
                                full_cat_ph,
                                &pk_bytes,
                                sk,
                                fee_puzzlehash,
                                &fee_address_str,
                                &synthetic_sk,
                                &synthetic_pk,
                            )
                            .await?;
                            return Ok(());
                        }
                        eprintln!(
                            "Peer rejected subscription due to chain reorg (attempt {reorg_retries}/3): {e}"
                        );
                        eprintln!("Waiting 15 seconds for reorg to settle, then retrying instant-react…");
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    } else {
                        eprintln!("Instant-react error: {e}");
                        eprintln!("Falling back to polling mode");
                        mine_polling(
                            &clients,
                            &config,
                            inner_puzzle_hash,
                            full_cat_ph,
                            &pk_bytes,
                            sk,
                            fee_puzzlehash,
                            &fee_address_str,
                            &synthetic_sk,
                            &synthetic_pk,
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        }
    } else {
        println!("Polling mode — checking every {:.0}s", config.default_sleep_secs);
        println!();
        mine_polling(
            &clients,
            &config,
            inner_puzzle_hash,
            full_cat_ph,
            &pk_bytes,
            sk,
            fee_puzzlehash,
            &fee_address_str,
            &synthetic_sk,
            &synthetic_pk,
        )
        .await?;
    }

    Ok(())
}

// ── Polling-based mining loop ──────────────────────────────────────────

/// Cached spend bundle for a specific (coin_id, height) pair so the polling
/// loop never grinds the same nonce twice.
type CachedBundle = Option<(Bytes32, u32, SpendBundle)>;

/// Push a bundle, retrying on transient node states up to `COIN_NOT_READY_MAX_RETRIES` times.
///
/// Two retry conditions:
/// - `coin_not_ready` (UNKNOWN_UNSPENT): the node hasn't indexed the parent coin yet.
/// - `status == "PENDING"` (only when `retry_pending`): the node acknowledged receipt but
///   the tx may be silently dropped before reaching the mempool.  Resubmitting causes the
///   node to either confirm it properly, return mempool_conflict (already there), or
///   re-evaluate.  The instant-react loop disables this: PENDING there is a same-coin
///   conflict with a still-valid earlier bundle, which identical re-pushes never resolve.
async fn push_tx_with_retry(
    clients: &[Arc<dyn RpcClient>],
    bundle: &SpendBundle,
    label: &str,
    config: &Config,
    retry_pending: bool,
) -> crate::client::PushTxResult {
    let retry_secs = config.coin_not_ready_retry_secs;
    let max_retries = config.coin_not_ready_max_retries;
    let debug = config.debug;

    let mut result = push_tx_to_all(clients, bundle).await;
    // Track the best (most recent success) result seen across all attempts so
    // that a transport error on a *retry* never clobbers an earlier success.
    let mut best_result: Option<crate::client::PushTxResult> = if result.success {
        Some(result.clone())
    } else {
        None
    };

    for attempt in 1..=max_retries {
        let is_pending = retry_pending
            && result.success
            && result.status.as_deref().map(|s| s.eq_ignore_ascii_case("pending")).unwrap_or(false);
        let is_not_ready = !result.success && result.error_category == "coin_not_ready";
        let is_rate_limited = !result.success && result.error_category == "rate_limit";

        if is_rate_limited {
            eprintln!(
                "{label}: rate limited by node (attempt {attempt}/{max_retries}), sleeping {ERROR_SLEEP_SECS}s…"
            );
            tokio::time::sleep(Duration::from_secs_f64(ERROR_SLEEP_SECS)).await;
            result = push_tx_to_all(clients, bundle).await;
            if result.success {
                best_result = Some(result.clone());
            }
            continue;
        }

        if !is_pending && !is_not_ready {
            break;
        }

        let reason = if is_pending { "PENDING (unconfirmed)" } else { "UNKNOWN_UNSPENT" };
        if debug {
            println!(
                "[debug] {label}: {reason} (attempt {attempt}/{max_retries}), retrying in {retry_secs}s…"
            );
        } else {
            eprintln!(
                "{label}: {reason} — retrying in {retry_secs}s (attempt {attempt}/{max_retries})"
            );
        }
        tokio::time::sleep(Duration::from_secs_f64(retry_secs)).await;
        result = push_tx_to_all(clients, bundle).await;

        if result.success {
            best_result = Some(result.clone());
        }

        // If a retry itself returns a transport error, log it immediately and
        // stop retrying — hammering further will not help and obscures the real
        // state.  We will return `best_result` below if we had an earlier success.
        if !result.success && result.error_category == "transport" {
            let is_decode_err = result
                .error
                .as_deref()
                .map_or(false, |e| e.contains("error decoding response body"));
            eprintln!(
                    "{label}: transport error on retry attempt {attempt}/{max_retries}: {:?}",
                result.error
            );
            if is_decode_err {
                eprintln!(
                    "  note: 'error decoding response body' often means the node returned a \
                     non-JSON response (rate-limit, HTML error page, or already-in-mempool rejection)"
                );
            }
            for (i, summary) in &result.per_client_errors {
                eprintln!("  client[{i}]: {summary}");
            }
            break;
        }
    }

    // Return the best successful result seen, or the final result if nothing succeeded.
    best_result.unwrap_or(result)
}

async fn mine_polling(
    clients: &[Arc<dyn RpcClient>],
    config: &Config,
    inner_puzzle_hash: Bytes32,
    full_cat_ph: Bytes32,
    pk_bytes: &[u8; 48],
    sk: &SecretKey,
    fee_puzzlehash: Bytes32,
    fee_address: &str,
    synthetic_sk: &SecretKey,
    synthetic_pk: &PublicKey,
) -> Result<()> {
    let mut submitted_coins: SubmittedCoins = HashMap::new();
    let mut last_height: i64 = -1;
    let mut cached_bundle: CachedBundle = None;

    loop {
        match poll_once(
            clients,
            config,
            inner_puzzle_hash,
            full_cat_ph,
            pk_bytes,
            sk,
            fee_puzzlehash,
            fee_address,
            synthetic_sk,
            synthetic_pk,
            &mut submitted_coins,
            &mut last_height,
            &mut cached_bundle,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Error in mining loop: {e}");
                let jitter = rand::random::<f64>() * 0.5 + 0.5;
                tokio::time::sleep(Duration::from_secs_f64(ERROR_SLEEP_SECS * jitter)).await;
            }
        }
        tokio::time::sleep(Duration::from_secs_f64(config.default_sleep_secs)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn poll_once(
    clients: &[Arc<dyn RpcClient>],
    config: &Config,
    inner_puzzle_hash: Bytes32,
    full_cat_ph: Bytes32,
    pk_bytes: &[u8; 48],
    sk: &SecretKey,
    fee_puzzlehash: Bytes32,
    _fee_address: &str,
    synthetic_sk: &SecretKey,
    synthetic_pk: &PublicKey,
    submitted_coins: &mut SubmittedCoins,
    last_height: &mut i64,
    cached_bundle: &mut CachedBundle,
) -> Result<()> {
    // Get blockchain state
    let mut active_client_idx = 0;
    let mut blockchain_state = None;
    for (i, c) in clients.iter().enumerate() {
        match c.get_blockchain_state().await {
            Ok(res) if res.success => {
                active_client_idx = i;
                blockchain_state = Some(res);
                break;
            }
            _ => continue,
        }
    }
    let bs = blockchain_state.context("Failed to get blockchain state from any client")?;
    let state = bs
        .blockchain_state
        .context("No blockchain_state in response")?;
    let height = state.peak.height;
    // Timelocks are evaluated against the last transaction block, not the peak.
    let last_tx = last_tx_height(&state.peak);

    let new_height = height as i64 != *last_height;
    if new_height {
        let is_first = *last_height < 0;
        *last_height = height as i64;
        if is_first || height % 100 == 0 {
            println!("Height: {height}");
        }
        // Check previously submitted coins
        if !submitted_coins.is_empty() {
            check_mining_results(
                clients[active_client_idx].as_ref(),
                inner_puzzle_hash,
                submitted_coins,
                &config.target_puzzlehash,
                &config.target_address,
            )
            .await;
        }
    }

    // Search for unspent lode coins
    let mut unspent_records = None;
    for c in clients {
        match c
            .get_coin_records_by_puzzle_hash(
                full_cat_ph,
                Some(GENESIS_HEIGHT),
                Some(height + 5),
                Some(false),
            )
            .await
        {
            Ok(res) if res.success => {
                unspent_records = Some(res);
                break;
            }
            Ok(res) => {
                eprintln!(
                    "get_coin_records_by_puzzle_hash failed: success=false, error={:?}",
                    res.error
                );
            }
            Err(e) => {
                eprintln!("get_coin_records_by_puzzle_hash exception: {e}");
            }
        }
    }
    let records = unspent_records.context("Failed to discover unspent coins on any client")?;
    let coin_records = records.coin_records.unwrap_or_default();
    if coin_records.is_empty() {
        if new_height && (config.debug || height % 50 == 0) {
            println!(
                "Height {height}: no unspent lode coins found (cat_ph={}…)",
                &hex::encode(full_cat_ph)
            );
        }
        return Ok(());
    }

    if config.debug {
        println!(
            "Height {height}: found {} unspent lode coin(s), best amount={}",
            coin_records.len(),
            coin_records.iter().map(|r| r.coin.amount).max().unwrap_or(0)
        );
    }

    let mine_height = last_tx;
    if mine_height < GENESIS_HEIGHT {
        println!(
            "Waiting for genesis. {} blocks to go!",
            GENESIS_HEIGHT - mine_height
        );
        return Ok(());
    }

    // Pick the best coin (largest, most recently confirmed)
    let max_amount = coin_records.iter().map(|r| r.coin.amount).max().unwrap_or(0);
    let viable: Vec<_> = coin_records
        .iter()
        .filter(|r| r.coin.amount as f64 >= max_amount as f64 * 0.9)
        .collect();
    let largest_cr = viable
        .iter()
        .max_by_key(|r| r.confirmed_block_index)
        .context("No viable coin records")?;

    let coin_id_key = largest_cr.coin.coin_id();

    // Skip if already submitted for this coin at the current pinned height;
    // a new transaction block changes the pinned height and needs a fresh spend.
    if submitted_coins.get(&coin_id_key) == Some(&mine_height) {
        if config.debug {
            println!(
                "[debug] Skipping resubmit for coin already submitted at height {mine_height}"
            );
        }
        return Ok(());
    }

    // ASSERT_HEIGHT_RELATIVE 1: the coin must be older than the last tx block.
    if largest_cr.confirmed_block_index >= mine_height {
        return Ok(());
    }

    let epoch = get_epoch(mine_height);
    let reward = get_reward(epoch);
    let difficulty_bits = get_difficulty_bits(epoch);

    if largest_cr.coin.amount < reward {
        println!(
            "Lode coin amount ({}) less than reward ({}), skipping",
            largest_cr.coin.amount, reward
        );
        return Ok(());
    }

    // Reuse cached bundle if coin_id and height match, otherwise grind fresh
    let use_cache = matches!(cached_bundle, Some((cid, ch, _)) if *cid == coin_id_key && *ch == mine_height);
    let bundle = if use_cache {
        if config.debug {
            println!("[debug] Reusing cached bundle for height {mine_height}");
        }
        cached_bundle.as_ref().unwrap().2.clone()
    } else {
        build_polling_bundle(
            clients,
            config,
            &largest_cr.coin,
            inner_puzzle_hash,
            full_cat_ph,
            pk_bytes,
            sk,
            fee_puzzlehash,
            synthetic_sk,
            synthetic_pk,
            mine_height,
            difficulty_bits,
            epoch,
            reward,
            cached_bundle,
            coin_id_key,
        )
        .await?
    };

    let result = push_tx_with_retry(clients, &bundle, "polling", config, true).await;
    if result.success {
        submitted_coins.insert(coin_id_key, mine_height);
        // Prune stale entries
        submitted_coins.retain(|_, v| height < *v + 10);
        println!(
            "Submitted mining spend bundle for height {mine_height}, Status={:?}",
            result.status
        );
    } else {
        match result.error_category {
            "already_spent" => {
                // Coin already confirmed-spent on-chain — a rival won.
                // Don't insert into submitted_coins; let check_mining_results
                // detect the loss on the next new height.
                eprintln!(
                    "Polling push rejected: coin {} already spent on-chain (rival win) at height {mine_height}",
                    hex::encode(coin_id_key)
                );
            }
            "transport" => {
                eprintln!(
                    "Failed to push tx (transport error) for coin {} at height {mine_height}: {:?}",
                    hex::encode(coin_id_key),
                    result.error
                );
                for (i, summary) in &result.per_client_errors {
                    eprintln!("  client[{i}]: {summary}");
                }
            }
            "mempool_conflict" => {
                if config.debug {
                    println!(
                        "[debug] Submit skipped (rival spend in mempool) for height {mine_height}: {:?}",
                        result.error
                    );
                }
            }
            cat => eprintln!(
                "Failed to submit mining spend bundle: {:?} [{cat}]",
                result.error
            ),
        }
    }

    Ok(())
}

// ── Polling bundle builder (grind nonce + build, then cache) ──────────

#[allow(clippy::too_many_arguments)]
async fn build_polling_bundle(
    clients: &[Arc<dyn RpcClient>],
    config: &Config,
    coin: &chia_protocol::Coin,
    inner_puzzle_hash: Bytes32,
    full_cat_ph: Bytes32,
    pk_bytes: &[u8; 48],
    sk: &SecretKey,
    fee_puzzlehash: Bytes32,
    synthetic_sk: &SecretKey,
    synthetic_pk: &PublicKey,
    mine_height: u32,
    difficulty_bits: u32,
    epoch: u32,
    reward: u64,
    cached_bundle: &mut CachedBundle,
    coin_id_key: Bytes32,
) -> Result<SpendBundle> {
    let _ = (full_cat_ph, epoch, reward); // used by caller for guards already

    println!(
        "Grinding nonce at height {mine_height} (epoch={epoch}, reward={reward}, difficulty=2^{difficulty_bits})…"
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let grind_start = Instant::now();
    let nonce = find_valid_nonce(
        &inner_puzzle_hash,
        pk_bytes,
        mine_height,
        difficulty_bits,
        MAX_NONCE_ATTEMPTS,
        config.thread_count,
        cancel,
    );
    let nonce = match nonce {
        Some(n) => n,
        None => {
            anyhow::bail!("Could not find valid nonce for height {mine_height}");
        }
    };
    println!("Found nonce {nonce} in {:.2?} — building spend bundle…", grind_start.elapsed());

    let target_cat = bootstrap_cat_from_coin(clients, coin, inner_puzzle_hash).await?;
    let target_cat = match target_cat {
        Some(c) => c,
        None => {
            anyhow::bail!("Could not reconstruct CAT lineage for coin");
        }
    };

    let fee_coins = if config.fee_mojos > 0 {
        fetch_fee_coins(clients, fee_puzzlehash, mine_height).await
    } else {
        Vec::new()
    };

    let bundle = build_mining_bundle(
        config,
        &target_cat,
        mine_height,
        nonce,
        inner_puzzle_hash,
        pk_bytes,
        sk,
        &fee_coins,
        fee_puzzlehash,
        synthetic_sk,
        synthetic_pk,
    )?;

    // Cache for subsequent polls at the same (coin_id, height)
    *cached_bundle = Some((coin_id_key, mine_height, bundle.clone()));

    Ok(bundle)
}

// ── Instant-react mining (LOCAL_FULL_NODE only) ────────────────────────
//
// Driven entirely by NewPeakWallet events.  On each new peak we fetch the
// current unspent lode coin via RPC, pin a spend to the last transaction
// block height, and fire it (nonces are cached/pre-ground per height).

#[allow(clippy::too_many_arguments)]
async fn mine_instant_react(
    clients: &[Arc<dyn RpcClient>],
    config: &Config,
    inner_puzzle_hash: Bytes32,
    full_cat_ph: Bytes32,
    pk_bytes: &[u8; 48],
    sk: &SecretKey,
    fee_puzzlehash: Bytes32,
    fee_address: &str,
    synthetic_sk: &SecretKey,
    synthetic_pk: &PublicKey,
) -> Result<()> {
    let primary = &clients[0];

    // Get initial blockchain state
    let bs_res = primary.get_blockchain_state().await?;
    if !bs_res.success {
        anyhow::bail!("Failed to get blockchain state for instant-react init");
    }
    let state = bs_res
        .blockchain_state
        .context("No blockchain_state in response")?;
    let height = state.peak.height;
    let header_hash = state.peak.header_hash;

    println!("Initial height: {height}");

    // Connect Peer
    let peer_host = extract_peer_host(config);
    let socket_addr: SocketAddr = format!("{peer_host}:{}", config.peer_port)
        .parse()
        .context("Invalid peer address")?;
    println!("Connecting to Chia peer protocol at {socket_addr}…");

    let cert = ChiaCertificate::generate()
        .context("Failed to generate ephemeral TLS certificate for peer connection")?;
    let connector = create_rustls_connector(&cert)?;
    let options = PeerOptions::default();

    let (peer, mut receiver) =
        connect_peer(config.network_name.clone(), connector, socket_addr, options).await?;
    println!("Peer connected to {socket_addr}");

    // Bootstrap initial Cat from RPC
    let (initial_cat, _) =
        bootstrap_cat_from_rpc(clients, full_cat_ph, inner_puzzle_hash, height)
            .await?
            .context("Failed to bootstrap initial Cat object")?;
    println!(
        "Bootstrapped Cat: coin_id={}…, amount={}",
        &hex::encode(initial_cat.coin.coin_id()),
        initial_cat.coin.amount
    );

    let mut submitted_coins: SubmittedCoins = HashMap::new();
    let mut current_coin_id: Option<Bytes32> = Some(initial_cat.coin.coin_id());
    let mut current_height = height;

    // Coins we know are spent on-chain (confirmed via "already_spent" push
    // rejection or our own successful submission).  While the RPC node is
    // slow to reflect the spend, this prevents us from repeatedly firing
    // stale bundles and rebuilding grids for dead coins.
    let mut known_spent_coins: HashSet<Bytes32> = HashSet::new();

    // Fetch initial fee coins
    let mut fee_coins = if config.fee_mojos > 0 {
        let fc = fetch_fee_coins(clients, fee_puzzlehash, height).await;
        if !fc.is_empty() {
            println!("Cached {} fee coin(s)", fc.len());
        } else {
            println!(
                "Warning: FEE_MOJOS={} but no fee coins at {fee_address}",
                config.fee_mojos
            );
        }
        fc
    } else {
        Vec::new()
    };

    // Register as a wallet peer so we receive NewPeakWallet broadcasts.
    // We subscribe to the lode puzzle hash to keep the connection alive,
    // but we do NOT process CoinStateUpdate — all coin state is fetched
    // via RPC on each new peak for simplicity and reliability.
    let filters = CoinStateFilters::new(true, true, false, 0);
    let puzzle_hashes = vec![full_cat_ph];

    let sub_resp = peer
        .request_puzzle_state(
            puzzle_hashes,
            Some(height),
            header_hash,
            filters,
            true,
        )
        .await;
    match sub_resp {
        Ok(Ok(respond)) => {
            if config.debug {
                println!(
                    "[debug] subscription response: is_finished={}, {} coin_states",
                    respond.is_finished,
                    respond.coin_states.len(),
                );
            }
        }
        Ok(Err(reject)) => {
            anyhow::bail!("Peer rejected puzzle state subscription: {:?}", reject);
        }
        Err(e) => {
            anyhow::bail!("Peer request_puzzle_state transport error: {e}");
        }
    }

    // Speculative grid of fully built and signed bundles keyed by
    // (lode coin, pinned height), covering the current coin and its next three
    // descendants across the upcoming transaction-block heights.  Firing is a
    // lookup; a miss falls back to building on demand.  Nonces depend only on
    // the pinned height so they are cached separately.  The grid is dropped
    // whenever the fee coin set changes, since bundles embed fee coins.
    let mut nonce_cache: HashMap<u32, u64> = HashMap::new();
    let mut bundle_grid: HashMap<(Bytes32, u32), SpendBundle> = HashMap::new();
    let mut cache_fee_ids: Vec<Bytes32> = fee_coins.iter().map(|c| c.coin_id()).collect();
    cache_fee_ids.sort();

    println!("Instant-react mining active — waiting for NewPeakWallet events…");
    println!();

    // Event loop — driven entirely by NewPeakWallet
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(60), receiver.recv()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                anyhow::bail!("Peer disconnected");
            }
            Err(_) => continue, // Timeout, keep waiting
        };

        if msg.msg_type != ProtocolMessageTypes::NewPeakWallet {
            if config.debug {
                println!("[debug] Ignoring peer message type: {:?}", msg.msg_type);
            }
            continue;
        }

        let peak_received = Instant::now();
        let peak = NewPeakWallet::from_bytes(&msg.data)?;
        let new_height = peak.height;
        if new_height == current_height {
            continue;
        }

        // Sanity-check: reject heights that deviate too far from what we last
        // confirmed in either direction, so a fraudulent peer cannot make us
        // act on bogus chain state.
        const MAX_PEAK_JUMP: u32 = 20;
        const LOUD_PEAK_JUMP: u32 = 5;
        if current_height > 0 {
            if new_height > current_height + MAX_PEAK_JUMP {
                eprintln!(
                    "⚠️  SUSPICIOUS PEAK (forward): {new_height} vs current={current_height} (+{}) — IGNORING",
                    new_height - current_height
                );
                continue;
            }
            if new_height < current_height.saturating_sub(MAX_PEAK_JUMP) {
                eprintln!(
                    "⚠️  SUSPICIOUS PEAK (backward): {new_height} vs current={current_height} (-{}) — IGNORING",
                    current_height - new_height
                );
                continue;
            }
            if new_height > current_height + LOUD_PEAK_JUMP {
                eprintln!(
                    "⚠️  LARGE FORWARD JUMP: height {new_height} vs current={current_height} (+{})",
                    new_height - current_height
                );
            } else if new_height < current_height.saturating_sub(LOUD_PEAK_JUMP) {
                eprintln!(
                    "⚠️  LARGE REORG: height {new_height} vs current={current_height} (-{})",
                    current_height - new_height
                );
            }
        }

        println!("NewPeak: {new_height}");
        current_height = new_height;

        // ── Fetch current unspent lode coin via RPC ──────────────
        let (rpc_cat, rpc_confirmed) = match bootstrap_cat_from_rpc(
            clients,
            full_cat_ph,
            inner_puzzle_hash,
            current_height,
        )
        .await
        {
            Ok(Some(found)) => found,
            Ok(None) => {
                println!(
                    "Height {current_height}: no unspent lode coin found (RPC returned nothing)"
                );
                continue;
            }
            Err(e) => {
                eprintln!("RPC error fetching lode coin at height {current_height}: {e}");
                continue;
            }
        };

        let rpc_coin_id = rpc_cat.coin.coin_id();
        if current_coin_id != Some(rpc_coin_id) {
            println!(
                "Lode coin changed at height {current_height}: coin_id={}…, amount={}",
                &hex::encode(rpc_coin_id),
                rpc_cat.coin.amount
            );
            known_spent_coins.clear();
            if !submitted_coins.is_empty() {
                check_mining_results(
                    clients[0].as_ref(),
                    inner_puzzle_hash,
                    &mut submitted_coins,
                    &config.target_puzzlehash,
                    &config.target_address,
                )
                .await;
            }
            current_coin_id = Some(rpc_coin_id);
        }

        // Refresh fee coins; a changed set invalidates every cached bundle.
        if config.fee_mojos > 0 {
            fee_coins = fetch_fee_coins(clients, fee_puzzlehash, current_height).await;
            let mut ids: Vec<Bytes32> = fee_coins.iter().map(|c| c.coin_id()).collect();
            ids.sort();
            if ids != cache_fee_ids {
                bundle_grid.clear();
                cache_fee_ids = ids;
            }
        }

        // Chia checks ASSERT_HEIGHT_* against the last *transaction block*
        // height, not the peak.  Pinning the spend to that height keeps it
        // valid for the next transaction block (window is [U, U+2]).
        let Some(mine_height) = fetch_last_tx_height(clients).await else {
            eprintln!("Height {current_height}: could not read last transaction block height");
            continue;
        };
        if mine_height <= GENESIS_HEIGHT {
            continue;
        }
        let epoch = get_epoch(mine_height);
        let reward = get_reward(epoch);
        let diff_bits = get_difficulty_bits(epoch);

        // First lode coin in [rpc coin, child, grandchild, great-grandchild]
        // not known to be spent.  Descendants are only reached when the RPC
        // lags the chain.
        let mut candidate: Option<(Cat, usize)> = None;
        {
            let mut cand = rpc_cat.clone();
            for gen in 0..=3usize {
                if !known_spent_coins.contains(&cand.coin.coin_id()) {
                    candidate = Some((cand, gen));
                    break;
                }
                if cand.coin.amount < reward {
                    break;
                }
                cand = cand.child(inner_puzzle_hash, cand.coin.amount - reward);
            }
        }

        match candidate {
            None => {
                eprintln!(
                    "Height {current_height}: all tracked lode coins known-spent — clearing and re-reading from RPC"
                );
                known_spent_coins.clear();
            }
            Some((cat, gen)) => {
                let coin_id = cat.coin.coin_id();
                // ASSERT_HEIGHT_RELATIVE 1: the coin must be older than the
                // last transaction block, otherwise the spend can only sit pending.
                let too_young = gen == 0 && rpc_confirmed >= mine_height;
                if cat.coin.amount < reward {
                    println!(
                        "Lode coin amount ({}) less than reward ({reward}), skipping",
                        cat.coin.amount
                    );
                } else if too_young {
                    if config.debug {
                        println!(
                            "[debug] Coin confirmed at {rpc_confirmed} is not yet spendable at last tx block {mine_height}"
                        );
                    }
                } else if submitted_coins.get(&coin_id) == Some(&mine_height) {
                    if config.debug {
                        println!("[debug] Already submitted coin for pinned height {mine_height}");
                    }
                } else {
                    for attempt in 0..2 {
                        let key = (coin_id, mine_height);
                        let bundle = match bundle_grid.get(&key) {
                            Some(b) => b.clone(),
                            None => {
                                let Some(nonce) = get_nonce(
                                    &mut nonce_cache,
                                    config,
                                    inner_puzzle_hash,
                                    pk_bytes,
                                    mine_height,
                                    diff_bits,
                                )
                                .await
                                else {
                                    eprintln!("Could not find nonce for height {mine_height}");
                                    break;
                                };
                                match build_mining_bundle(
                                    config,
                                    &cat,
                                    mine_height,
                                    nonce,
                                    inner_puzzle_hash,
                                    pk_bytes,
                                    sk,
                                    &fee_coins,
                                    fee_puzzlehash,
                                    synthetic_sk,
                                    synthetic_pk,
                                ) {
                                    Ok(b) => {
                                        bundle_grid.insert(key, b.clone());
                                        b
                                    }
                                    Err(e) => {
                                        eprintln!("Bundle build error at height {mine_height}: {e}");
                                        break;
                                    }
                                }
                            }
                        };

                        println!(
                            "NewPeak {current_height}: firing bundle (pinned={mine_height}, coin={}…{}) +{}ms since peak",
                            &hex::encode(coin_id),
                            if gen > 0 { format!(" [descendant gen={gen}]") } else { String::new() },
                            peak_received.elapsed().as_millis()
                        );
                        let label = format!("NewPeak h={current_height} pinned={mine_height}");
                        let push_started = Instant::now();
                        // Descendants may not be indexed yet; don't block on retries.
                        let result = if gen > 0 {
                            push_tx_to_all(clients, &bundle).await
                        } else {
                            push_tx_with_retry(clients, &bundle, &label, config, false).await
                        };

                        if result.success {
                            submitted_coins.insert(coin_id, mine_height);
                            let detail = match &result.error {
                                Some(e) => format!(", Detail={e:?}"),
                                None => String::new(),
                            };
                            println!(
                                "Submitted mining spend for pinned height {mine_height}, Status={:?}{detail} (push took {}ms, {}ms since peak)",
                                result.status,
                                push_started.elapsed().as_millis(),
                                peak_received.elapsed().as_millis()
                            );
                            break;
                        }

                        match result.error_category {
                            "already_spent" => {
                                // DOUBLE_SPEND can name any input, including a
                                // fee coin.  Only trust it for the lode coin
                                // after confirming against chain state.
                                if coin_spent_on_chain(clients, coin_id).await {
                                    known_spent_coins.insert(coin_id);
                                    bundle_grid.retain(|(c, _), _| *c != coin_id);
                                    eprintln!(
                                        "Push rejected: lode coin {}… already spent on-chain",
                                        &hex::encode(coin_id)
                                    );
                                    if !submitted_coins.is_empty() {
                                        check_mining_results(
                                            clients[0].as_ref(),
                                            inner_puzzle_hash,
                                            &mut submitted_coins,
                                            &config.target_puzzlehash,
                                            &config.target_address,
                                        )
                                        .await;
                                    }
                                    break;
                                }
                                eprintln!(
                                    "Push got DOUBLE_SPEND but lode coin {}… is unspent — a bundled fee coin is stale: {:?}",
                                    &hex::encode(coin_id),
                                    result.error
                                );
                                bundle_grid.clear();
                                if config.fee_mojos > 0 && attempt == 0 {
                                    fee_coins =
                                        fetch_fee_coins(clients, fee_puzzlehash, current_height).await;
                                    let mut ids: Vec<Bytes32> =
                                        fee_coins.iter().map(|c| c.coin_id()).collect();
                                    ids.sort();
                                    if ids != cache_fee_ids {
                                        cache_fee_ids = ids;
                                        continue; // retry once with fresh fee coins
                                    }
                                }
                            }
                            "mempool_conflict" => {
                                if config.debug {
                                    println!(
                                        "[debug] Mempool conflict at height {current_height} — will retry next peak"
                                    );
                                }
                            }
                            "transport" => {
                                eprintln!(
                                    "Transport error pushing bundle (will retry next block): {:?}",
                                    result.error
                                );
                                for (i, summary) in &result.per_client_errors {
                                    eprintln!("  client[{i}]: {summary}");
                                }
                            }
                            "coin_not_ready" => {
                                if config.debug {
                                    println!(
                                        "[debug] Coin not ready (UNKNOWN_UNSPENT) — will retry next peak"
                                    );
                                }
                            }
                            cat => eprintln!("Push failed: {:?} [{cat}]", result.error),
                        }
                        break;
                    }
                }
            }
        }

        // ── Top up the speculative grid ──────────────────────────
        // The next transaction block lands 1–7 blocks ahead, and its pinned
        // height is that block's own height, so cover mine_height..=+7 for the
        // root coin and each descendant (gen=N exists once N ancestors are spent).
        let mut chain: Vec<Cat> = Vec::with_capacity(4);
        {
            let mut cand = rpc_cat.clone();
            for gen in 0..=3u32 {
                chain.push(cand.clone());
                let r = get_reward(get_epoch(mine_height + gen));
                if cand.coin.amount < r {
                    break;
                }
                cand = cand.child(inner_puzzle_hash, cand.coin.amount - r);
            }
        }
        let chain_ids: HashSet<Bytes32> = chain.iter().map(|c| c.coin.coin_id()).collect();
        bundle_grid.retain(|(c, h), _| chain_ids.contains(c) && *h + 10 >= current_height);
        let mut built = 0usize;
        for h in mine_height..=(mine_height + 7) {
            let missing: Vec<&Cat> = chain
                .iter()
                .enumerate()
                .filter(|(gen, cat)| {
                    // The root coin can never be spent at a pinned height at or
                    // below its confirmation height.
                    (*gen > 0 || h > rpc_confirmed)
                        && !bundle_grid.contains_key(&(cat.coin.coin_id(), h))
                })
                .map(|(_, cat)| cat)
                .collect();
            if missing.is_empty() {
                continue;
            }
            let Some(nonce) = get_nonce(
                &mut nonce_cache,
                config,
                inner_puzzle_hash,
                pk_bytes,
                h,
                get_difficulty_bits(get_epoch(h)),
            )
            .await
            else {
                continue;
            };
            for cat in missing {
                match build_mining_bundle(
                    config,
                    cat,
                    h,
                    nonce,
                    inner_puzzle_hash,
                    pk_bytes,
                    sk,
                    &fee_coins,
                    fee_puzzlehash,
                    synthetic_sk,
                    synthetic_pk,
                ) {
                    Ok(b) => {
                        bundle_grid.insert((cat.coin.coin_id(), h), b);
                        built += 1;
                    }
                    Err(e) => eprintln!("Grid build error at height {h}: {e}"),
                }
            }
        }
        if built > 0 && config.debug {
            println!("[debug] Grid top-up: +{built} bundles ({} total)", bundle_grid.len());
        }
        nonce_cache.retain(|h, _| *h + 10 >= current_height);
        submitted_coins.retain(|_, v| current_height < *v + 10);
    }
}

/// Grind (or fetch from cache) the nonce for a pinned height.
async fn get_nonce(
    cache: &mut HashMap<u32, u64>,
    config: &Config,
    inner_puzzle_hash: Bytes32,
    pk_bytes: &[u8; 48],
    height: u32,
    difficulty_bits: u32,
) -> Option<u64> {
    if let Some(n) = cache.get(&height) {
        return Some(*n);
    }
    let pkb = *pk_bytes;
    let threads = config.thread_count;
    let start = Instant::now();
    let nonce = tokio::task::spawn_blocking(move || {
        find_valid_nonce(
            &inner_puzzle_hash,
            &pkb,
            height,
            difficulty_bits,
            MAX_NONCE_ATTEMPTS,
            threads,
            Arc::new(AtomicBool::new(false)),
        )
    })
    .await
    .ok()
    .flatten()?;
    if config.debug {
        println!("[debug] Found nonce {nonce} for height {height} in {:.2?}", start.elapsed());
    }
    cache.insert(height, nonce);
    Some(nonce)
}

/// Height of the last transaction block as of `peak`.  Chia evaluates height
/// timelocks against this value rather than the peak height.
fn last_tx_height(peak: &BlockRecord) -> u32 {
    if peak.timestamp.is_some() {
        peak.height
    } else {
        peak.prev_transaction_block_height
    }
}

async fn fetch_last_tx_height(clients: &[Arc<dyn RpcClient>]) -> Option<u32> {
    for c in clients {
        if let Ok(res) = c.get_blockchain_state().await {
            if let (true, Some(state)) = (res.success, res.blockchain_state) {
                return Some(last_tx_height(&state.peak));
            }
        }
    }
    None
}

/// True only if some client positively reports the coin as spent.
async fn coin_spent_on_chain(clients: &[Arc<dyn RpcClient>], coin_id: Bytes32) -> bool {
    for c in clients {
        if let Ok(res) = c.get_coin_record_by_name(coin_id).await {
            if let (true, Some(cr)) = (res.success, res.coin_record) {
                return cr.spent;
            }
        }
    }
    false
}

// ── Cat bootstrap helpers ──────────────────────────────────────────────

async fn bootstrap_cat_from_coin(
    clients: &[Arc<dyn RpcClient>],
    coin: &Coin,
    inner_puzzle_hash: Bytes32,
) -> Result<Option<Cat>> {
    for c in clients {
        match bootstrap_cat_from_coin_with_client(c.as_ref(), coin, inner_puzzle_hash).await {
            Ok(Some(cat)) => return Ok(Some(cat)),
            _ => continue,
        }
    }
    Ok(None)
}

async fn bootstrap_cat_from_coin_with_client(
    client: &dyn RpcClient,
    coin: &Coin,
    inner_puzzle_hash: Bytes32,
) -> Result<Option<Cat>> {
    let parent_res = client.get_coin_record_by_name(coin.parent_coin_info).await?;
    if !parent_res.success {
        return Ok(None);
    }
    let parent_record = match parent_res.coin_record {
        Some(r) => r,
        None => return Ok(None),
    };

    let gps_res = client
        .get_puzzle_and_solution(
            parent_record.coin.coin_id(),
            Some(parent_record.spent_block_index),
        )
        .await?;
    if !gps_res.success {
        return Ok(None);
    }
    let coin_spend = match gps_res.coin_solution {
        Some(cs) => cs,
        None => return Ok(None),
    };

    parse_cat_children(&parent_record.coin, &coin_spend, inner_puzzle_hash, Some(coin))
}

async fn bootstrap_cat_from_rpc(
    clients: &[Arc<dyn RpcClient>],
    full_cat_ph: Bytes32,
    inner_puzzle_hash: Bytes32,
    height: u32,
) -> Result<Option<(Cat, u32)>> {
    let mut records = None;
    for c in clients {
        match c
            .get_coin_records_by_puzzle_hash(
                full_cat_ph,
                Some(GENESIS_HEIGHT),
                Some(height + 5),
                Some(false),
            )
            .await
        {
            Ok(res) if res.success && res.coin_records.is_some() => {
                records = Some(res);
                break;
            }
            _ => continue,
        }
    }

    let coin_records = records.and_then(|r| r.coin_records).unwrap_or_default();
    if coin_records.is_empty() {
        return Ok(None);
    }

    let max_amount = coin_records.iter().map(|r| r.coin.amount).max().unwrap_or(0);
    let viable: Vec<_> = coin_records
        .iter()
        .filter(|r| r.coin.amount as f64 >= max_amount as f64 * 0.9)
        .collect();
    let cr = viable.iter().max_by_key(|r| r.confirmed_block_index).unwrap();

    Ok(bootstrap_cat_from_coin(clients, &cr.coin, inner_puzzle_hash)
        .await?
        .map(|cat| (cat, cr.confirmed_block_index)))
}

/// Parse child CATs from a parent coin spend.
fn parse_cat_children(
    parent_coin: &Coin,
    coin_spend: &CoinSpend,
    inner_puzzle_hash: Bytes32,
    target_coin: Option<&Coin>,
) -> Result<Option<Cat>> {
    let mut allocator = Allocator::new();
    let puzzle_ptr =
        clvmr::serde::node_from_bytes(&mut allocator, coin_spend.puzzle_reveal.as_ref())?;
    let solution_ptr =
        clvmr::serde::node_from_bytes(&mut allocator, coin_spend.solution.as_ref())?;

    let puzzle = Puzzle::parse(&allocator, puzzle_ptr);

    let Some((cat_parsed, inner_puzzle, inner_solution)) =
        Cat::parse(&allocator, *parent_coin, puzzle, solution_ptr)?
    else {
        return Ok(None);
    };

    // Run the inner puzzle to get conditions
    let output = run_puzzle(&mut allocator, inner_puzzle.ptr(), inner_solution)?;
    let conditions: Vec<Condition> = FromClvm::from_clvm(&allocator, output)?;

    for condition in &conditions {
        if let Condition::CreateCoin(cc) = condition {
            let child_cat = cat_parsed.child(cc.puzzle_hash, cc.amount);

            if child_cat.info.p2_puzzle_hash == inner_puzzle_hash
                && child_cat.info.asset_id == CAT_TAIL_HASH
            {
                if let Some(target) = target_coin {
                    if child_cat.coin.coin_id() == target.coin_id() {
                        return Ok(Some(child_cat));
                    }
                } else {
                    return Ok(Some(child_cat));
                }
            }
        }
    }

    Ok(None)
}

async fn fetch_fee_coins(
    clients: &[Arc<dyn RpcClient>],
    fee_puzzlehash: Bytes32,
    height: u32,
) -> Vec<Coin> {
    for c in clients {
        match c
            .get_coin_records_by_puzzle_hash(
                fee_puzzlehash,
                Some(GENESIS_HEIGHT),
                Some(height + 5),
                Some(false),
            )
            .await
        {
            Ok(res) if res.success => {
                return res
                    .coin_records
                    .unwrap_or_default()
                    .iter()
                    .map(|r| r.coin)
                    .collect();
            }
            _ => continue,
        }
    }
    Vec::new()
}

// ── Mining result checking ─────────────────────────────────────────────

/// Check submitted coins for confirmed results (win/loss logging).
async fn check_mining_results(
    client: &dyn RpcClient,
    _inner_puzzle_hash: Bytes32,
    submitted_coins: &mut SubmittedCoins,
    target_puzzlehash: &Bytes32,
    target_address: &str,
) {
    let mut to_remove = Vec::new();
    let snapshot: Vec<(Bytes32, u32)> = submitted_coins.iter().map(|(&k, &v)| (k, v)).collect();

    for (coin_id, sub_height) in snapshot {
        match client.get_coin_record_by_name(coin_id).await {
            Ok(res) if res.success => {
                if let Some(cr) = res.coin_record {
                    if !cr.spent {
                        continue;
                    }
                    to_remove.push(coin_id);

                    match client
                        .get_puzzle_and_solution(coin_id, Some(cr.spent_block_index))
                        .await
                    {
                        Ok(gps_res) if gps_res.success => {
                            if let Some(cs) = gps_res.coin_solution {
                                let mut allocator = Allocator::new();
                                let Ok(puzzle_ptr) = clvmr::serde::node_from_bytes(
                                    &mut allocator,
                                    cs.puzzle_reveal.as_ref(),
                                ) else {
                                    continue;
                                };
                                let Ok(solution_ptr) = clvmr::serde::node_from_bytes(
                                    &mut allocator,
                                    cs.solution.as_ref(),
                                ) else {
                                    continue;
                                };

                                let puzzle = Puzzle::parse(&allocator, puzzle_ptr);
                                if let Ok(Some((_, inner_puz, inner_sol))) =
                                    Cat::parse(&allocator, cr.coin, puzzle, solution_ptr)
                                {
                                    if let Ok(output) =
                                        run_puzzle(&mut allocator, inner_puz.ptr(), inner_sol)
                                    {
                                        if let Ok(conditions) =
                                            <Vec<Condition>>::from_clvm(&allocator, output)
                                        {
                                            let winning_pinned = conditions.iter().find_map(|c| {
                                                if let Condition::AssertHeightAbsolute(a) = c {
                                                    Some(a.height)
                                                } else {
                                                    None
                                                }
                                            });
                                            let mut reward_mojos = 0u64;
                                            for cond in &conditions {
                                                if let Condition::CreateCoin(cc) = cond {
                                                    if cc.puzzle_hash == *target_puzzlehash {
                                                        reward_mojos = cc.amount;
                                                        break;
                                                    }
                                                }
                                            }
                                            if reward_mojos > 0 {
                                                println!("{EXCAVATOR_ART}");
                                                println!(
                                                    "Win CONFIRMED at height {} (winning bundle pinned={}; last submitted pinned={sub_height})!",
                                                    cr.spent_block_index,
                                                    winning_pinned
                                                        .map_or("?".to_string(), |h| h.to_string())
                                                );
                                                let reward_cat = reward_mojos as f64 / 1000.0;
                                                println!(
                                                    "Reward of {reward_cat:.3} XKV8 sent to {target_address}"
                                                );
                                                println!();
                                            } else {
                                                println!(
                                                    "Coin submitted at height {} was mined by another miner at height {}",
                                                    sub_height, cr.spent_block_index
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            eprintln!(
                                "Could not retrieve puzzle/solution for {}…",
                                &hex::encode(coin_id)
                            );
                        }
                    }
                } else {
                    to_remove.push(coin_id);
                }
            }
            _ => {
                to_remove.push(coin_id);
            }
        }
    }

    for coin_id in to_remove {
        submitted_coins.remove(&coin_id);
    }

}

// ── Helper: extract peer host ──────────────────────────────────────────

fn extract_peer_host(config: &Config) -> String {
    let val = config
        .local_full_node
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();

    let stripped = val
        .strip_prefix("https://")
        .or_else(|| val.strip_prefix("http://"))
        .unwrap_or(&val)
        .to_string();

    let host = if stripped.contains(':') {
        stripped.split(':').next().unwrap_or(&stripped).to_string()
    } else {
        stripped
    };

    if host.is_empty() || ["1", "true", "yes", "on"].contains(&host.to_lowercase().as_str()) {
        return "127.0.0.1".to_string();
    }

    if host.to_lowercase() == "localhost" {
        return "127.0.0.1".to_string();
    }

    use std::net::ToSocketAddrs;
    if let Ok(mut addrs) = format!("{host}:0").to_socket_addrs() {
        if let Some(addr) = addrs.next() {
            return addr.ip().to_string();
        }
    }

    host
}

//! Main mining loops: polling-based and instant-react (Peer subscription).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chia_bls::{PublicKey, SecretKey};
use chia_protocol::{
    Bytes32, Coin, CoinSpend, CoinStateFilters,
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
/// - `status == "PENDING"`: the node acknowledged receipt but the tx may be silently
///   dropped before reaching the mempool.  Resubmitting causes the node to either
///   confirm it properly, return mempool_conflict (already there), or re-evaluate.
async fn push_tx_with_retry(
    clients: &[Arc<dyn RpcClient>],
    bundle: &SpendBundle,
    label: &str,
    config: &Config,
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
        let is_pending = result.success
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

    let mine_height = height;
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

    // Skip entirely if already submitted for this coin
    if submitted_coins.contains_key(&coin_id_key) {
        if config.debug {
            println!(
                "[debug] Skipping resubmit for coin already submitted at height {mine_height}"
            );
        }
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

    let result = push_tx_with_retry(clients, &bundle, "polling", config).await;
    if result.success {
        submitted_coins.insert(coin_id_key, mine_height);
        // Prune stale entries
        submitted_coins.retain(|_, v| mine_height < *v + 3);
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

    let user_height = mine_height - 1;
    println!(
        "Grinding nonce at height {mine_height} (user_height={user_height}, epoch={epoch}, reward={reward}, difficulty=2^{difficulty_bits})…"
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let grind_start = Instant::now();
    let nonce = find_valid_nonce(
        &inner_puzzle_hash,
        pk_bytes,
        user_height,
        difficulty_bits,
        MAX_NONCE_ATTEMPTS,
        config.thread_count,
        cancel,
    );
    let nonce = match nonce {
        Some(n) => n,
        None => {
            anyhow::bail!("Could not find valid nonce for height {mine_height} (user_height={user_height})");
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
        user_height,
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
// current unspent lode coin via RPC, invalidate stale grid entries, fire
// the best precomputed bundle, and rebuild the grid when it is depleted.

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
    let initial_cat =
        bootstrap_cat_from_rpc(clients, full_cat_ph, inner_puzzle_hash, height).await?;
    let initial_cat = initial_cat.context("Failed to bootstrap initial Cat object")?;
    println!(
        "Bootstrapped Cat: coin_id={}…, amount={}",
        &hex::encode(initial_cat.coin.coin_id()),
        initial_cat.coin.amount
    );

    let mut submitted_coins: SubmittedCoins = HashMap::new();
    let mut current_cat: Option<Cat> = Some(initial_cat);
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

    // Start with an empty grid — the first NewPeakWallet message (which
    // arrives within seconds) will trigger the initial grid build.
    let mut bundle_grid: Vec<PrecomputedBundle> = Vec::new();

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

        let peak = NewPeakWallet::from_bytes(&msg.data)?;
        let new_height = peak.height;
        if new_height == current_height {
            continue;
        }

        println!("NewPeak: {new_height}");
        current_height = new_height;

        // ── Fetch current unspent lode coin via RPC ──────────────
        let rpc_cat = bootstrap_cat_from_rpc(
            clients,
            full_cat_ph,
            inner_puzzle_hash,
            current_height,
        )
        .await;

        let rpc_cat = match rpc_cat {
            Ok(Some(cat)) => cat,
            Ok(None) => {
                if config.debug || current_height % 50 == 0 {
                    println!(
                        "Height {current_height}: no unspent lode coin found"
                    );
                }
                continue;
            }
            Err(e) => {
                eprintln!("RPC error fetching lode coin at height {current_height}: {e}");
                continue;
            }
        };

        let rpc_coin_id = rpc_cat.coin.coin_id();
        let prev_coin_id = current_cat.as_ref().map(|c| c.coin.coin_id());
        let coin_changed = prev_coin_id.map_or(true, |prev| prev != rpc_coin_id);

        if coin_changed {
            println!(
                "Lode coin changed at height {current_height}: coin_id={}…, amount={}",
                &hex::encode(rpc_coin_id),
                rpc_cat.coin.amount
            );

            // RPC finally shows a new coin — clear known-spent tracking
            // for old coins since they are no longer relevant.
            known_spent_coins.clear();

            // Check if any of our submitted coins were confirmed
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

            // Discard grid entries for ancestors (any coin_id that isn't
            // the new coin or its descendants).  Since the coin changed,
            // all entries targeting the old coin are stale.
            let before = bundle_grid.len();
            bundle_grid.retain(|p| p.target_coin_id == rpc_coin_id);
            let discarded = before - bundle_grid.len();
            if discarded > 0 && config.debug {
                println!(
                    "[debug] Discarded {discarded} grid entries for old coin(s) ({} remain)",
                    bundle_grid.len()
                );
            }

            current_cat = Some(rpc_cat.clone());
        }

        let rpc_coin_is_spent = known_spent_coins.contains(&rpc_coin_id);

        // Prune grid entries whose target_height is below the current height
        // (they can no longer land in a valid block).
        let before = bundle_grid.len();
        bundle_grid.retain(|p| p.target_height >= current_height);
        let pruned = before - bundle_grid.len();
        if pruned > 0 && config.debug {
            println!(
                "[debug] Pruned {pruned} expired grid entries ({} remain)",
                bundle_grid.len()
            );
        }

        // Refresh fee coins via RPC
        if config.fee_mojos > 0 {
            fee_coins = fetch_fee_coins(clients, fee_puzzlehash, current_height).await;
        }

        // ── Fire best precomputed bundle ─────────────────────────
        // Always fire on every NewPeak, even if we previously submitted
        // for this coin.  This ensures our target height stays fresh —
        // if the chain advances past our previous target, the old spend
        // ages out of the mempool and we need a replacement immediately.
        //
        // When the RPC coin is known-spent, we fire the best DESCENDANT
        // entry from the grid (gen=1+) instead — these target child coins
        // that should appear once the on-chain spend confirms.
        {
            let best = if rpc_coin_is_spent {
                // RPC is stale — fire best descendant entry (any coin_id
                // that isn't the spent one).
                bundle_grid
                    .iter()
                    .filter(|p| {
                        p.target_coin_id != rpc_coin_id
                            && p.target_height >= current_height
                            && p.target_height <= current_height + 2
                    })
                    .min_by_key(|p| p.target_height)
                    .map(|p| (p.bundle.clone(), p.target_height, p.nonce, p.target_coin_id))
            } else {
                // Normal path — fire entry matching the current RPC coin.
                bundle_grid
                    .iter()
                    .filter(|p| {
                        p.target_coin_id == rpc_coin_id
                            && p.target_height >= current_height
                            && p.target_height <= current_height + 2
                    })
                    .min_by_key(|p| p.target_height)
                    .map(|p| (p.bundle.clone(), p.target_height, p.nonce, p.target_coin_id))
            };

            if let Some((bundle, target_height, nonce, fire_coin_id)) = best {
                println!(
                    "NewPeak {current_height}: firing precomputed bundle (target_height={target_height}, nonce={nonce}, coin={}…{})",
                    &hex::encode(fire_coin_id),
                    if rpc_coin_is_spent { " [descendant]" } else { "" }
                );
                let label = format!("NewPeak h={current_height} target={target_height}");
                // For descendant coins (parent known-spent), the child may
                // not be indexed yet.  Use a single attempt without retries
                // to avoid blocking on UNKNOWN_UNSPENT — the next NewPeak
                // will try again once the node has caught up.
                let result = if rpc_coin_is_spent {
                    push_tx_to_all(clients, &bundle).await
                } else {
                    push_tx_with_retry(clients, &bundle, &label, config).await
                };
                if result.success {
                    // Track for win/loss detection, but do NOT mark as
                    // known-spent or purge grid — we keep firing fresh
                    // heights on each subsequent peak so our spend never
                    // ages out of the mempool.
                    submitted_coins.insert(fire_coin_id, target_height);
                    println!(
                        "Submitted mining spend for height {target_height}, Status={:?}",
                        result.status
                    );
                } else {
                    match result.error_category {
                        "already_spent" => {
                            // Coin confirmed-spent on-chain (our win or rival).
                            // Mark as known-spent so we fire descendants
                            // instead on subsequent peaks.
                            known_spent_coins.insert(fire_coin_id);
                            bundle_grid.retain(|p| p.target_coin_id != fire_coin_id);
                            submitted_coins.remove(&fire_coin_id);
                            eprintln!(
                                "Push rejected: coin {}… already spent on-chain",
                                &hex::encode(fire_coin_id)
                            );
                            // Eagerly check if it was actually our win.
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
                        }
                        "mempool_conflict" => {
                            // Our own or a rival spend is already in the
                            // mempool.  Keep firing on subsequent peaks —
                            // the existing entry may age out.
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
                            // Child coin not indexed yet — expected for
                            // descendants.  Will retry on next NewPeak.
                            if config.debug {
                                println!(
                                    "[debug] Coin not ready (UNKNOWN_UNSPENT) — will retry next peak"
                                );
                            }
                        }
                        cat => eprintln!("Push failed: {:?} [{cat}]", result.error),
                    }
                }
            }
        }

        // ── Rebuild grid if depleted ─────────────────────────────
        // When the RPC coin is known-spent, count ALL remaining entries
        // (descendants) — don't rebuild until those are exhausted too.
        let usable_entries = if rpc_coin_is_spent {
            bundle_grid
                .iter()
                .filter(|p| p.target_height >= current_height)
                .count()
        } else {
            bundle_grid
                .iter()
                .filter(|p| p.target_coin_id == rpc_coin_id && p.target_height >= current_height)
                .count()
        };

        // Skip rebuild when RPC is stale — we'd just rebuild for the
        // spent coin again.  Wait for a fresh coin to appear.
        if usable_entries == 0 && !rpc_coin_is_spent {
            println!(
                "Height {current_height}: grid empty for current coin — rebuilding"
            );
            let cfg = config.clone();
            let cat = current_cat.clone();
            let iph = inner_puzzle_hash;
            let pkb = *pk_bytes;
            let sk2 = sk.clone();
            let fph = fee_puzzlehash;
            let ssk = synthetic_sk.clone();
            let spk = *synthetic_pk;
            let fc = fee_coins.clone();
            let ch = current_height;
            bundle_grid = tokio::task::spawn_blocking(move || {
                precompute_bundle_grid(
                    &cfg, &cat, ch, iph, &pkb, &sk2, fph, &ssk, &spk, &fc,
                )
            })
            .await?;
        }

        // Prune stale submitted coins
        submitted_coins.retain(|_, v| current_height < *v + 5);
    }
}

// ── Precomputed bundle grid ────────────────────────────────────────────
//
// We speculatively build a 4 (coin generations) × 5 (mine heights) grid
// with gen-shifting applied.
// Rows  : root, child, grandchild, great-grandchild of the current lode coin.
// Cols  : current_height + offset (shifted by generation).
//
// This covers the most common race conditions:
//   • The chain advances a block or two before our update arrives.
//   • The lode coin we track turns out to be a grandparent of what the chain
//     actually confirmed (because another miner fired first).

struct PrecomputedBundle {
    target_height: u32,
    target_coin_id: Bytes32,
    bundle: SpendBundle,
    nonce: u64,
}

/// Build a 4×5 grid of precomputed spend bundles (gen-shifted).
///
/// Rows  (coin axis) : root → child → grandchild → great-grandchild of `current_cat`.
/// Cols  (height axis): current_height + offset (shifted by gen so gen=N starts at +N+1).
///
/// Each cell is independent: a different nonce is ground for every
/// (coin_id, mine_height) pair so that whichever combination the chain
/// actually presents can be pushed immediately.
///
/// Entries that cannot be built (e.g. insufficient amount, nonce not found)
/// are silently skipped — the returned `Vec` may have fewer than 20 entries.
#[allow(clippy::too_many_arguments)]
fn precompute_bundle_grid(
    config: &Config,
    current_cat: &Option<Cat>,
    current_height: u32,
    inner_puzzle_hash: Bytes32,
    pk_bytes: &[u8; 48],
    sk: &SecretKey,
    fee_puzzlehash: Bytes32,
    synthetic_sk: &SecretKey,
    synthetic_pk: &PublicKey,
    fee_coins: &[Coin],
) -> Vec<PrecomputedBundle> {
    const COIN_GENERATIONS: usize = 3;
    // 5 height offsets per generation.  With the gen-shift (gen=N adds +N),
    // the effective ranges are:
    //   gen=0: current_height + 1..5    (root coin, immediate)
    //   gen=1: current_height + 2..6    (child, ~3-6 blocks to appear)
    //   gen=2: current_height + 3..7    (grandchild)
    //   gen=3: current_height + 4..8    (great-grandchild)
    // Each nonce grind takes ~5-50ms, so 4×5 = 20 entries ≈ 0.4s total.
    const HEIGHT_OFFSETS: [u32; 5] = [1, 2, 3, 4, 5];

    let root_cat = match current_cat.as_ref() {
        Some(c) => c.clone(),
        None => return Vec::new(),
    };

    // Total entries: (COIN_GENERATIONS + 1) coins × HEIGHT_OFFSETS heights.
    // gen=0: spend root_cat itself (sole-miner first move).
    // gen=1..=COIN_GENERATIONS: spend child, grandchild, great-grandchild
    //   (used when another miner or a prior spend has already created the child).
    let mut grid: Vec<PrecomputedBundle> =
        Vec::with_capacity((COIN_GENERATIONS + 1) * HEIGHT_OFFSETS.len());

    // --- gen=0: spend root_cat (the current unspent lode coin) ----------
    {
        let base_epoch = get_epoch(current_height);
        let base_reward = get_reward(base_epoch);
        if root_cat.coin.amount >= base_reward {
            for &height_offset in &HEIGHT_OFFSETS {
                let target_height = current_height + height_offset;
                let user_height = target_height - 1;
                let epoch = get_epoch(target_height);
                let diff_bits = get_difficulty_bits(epoch);

                let cancel = Arc::new(AtomicBool::new(false));
                let grind_start = Instant::now();
                let nonce = match find_valid_nonce(
                    &inner_puzzle_hash,
                    pk_bytes,
                    user_height,
                    diff_bits,
                    MAX_NONCE_ATTEMPTS,
                    config.thread_count,
                    cancel,
                ) {
                    Some(n) => n,
                    None => {
                        if config.debug {
                            println!("[debug] precompute_grid: gen=0 h={target_height} user_h={user_height} — nonce not found, skipping");
                        }
                        continue;
                    }
                };
                println!(
                    "Found nonce {nonce} in {:.2?} (precomputed gen=0 height={target_height}, user_height={user_height})",
                    grind_start.elapsed()
                );

                let bundle = match build_mining_bundle(
                    config,
                    &root_cat,
                    user_height,
                    nonce,
                    inner_puzzle_hash,
                    pk_bytes,
                    sk,
                    fee_coins,
                    fee_puzzlehash,
                    synthetic_sk,
                    synthetic_pk,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("precompute_grid: gen=0 h={target_height} user_h={user_height} bundle build error: {e}");
                        continue;
                    }
                };

                let coin_id = root_cat.coin.coin_id();
                println!(
                    "Precomputed bundle ready for height {target_height} (user_height={user_height}, nonce={nonce}, coin={}…, gen=0)",
                    &hex::encode(coin_id)
                );
                grid.push(PrecomputedBundle { target_height, target_coin_id: coin_id, bundle, nonce });
            }
        } else if config.debug {
            println!(
                "[debug] precompute_grid: gen=0 root coin amount ({}) < reward ({}), skipping root row",
                root_cat.coin.amount, base_reward
            );
        }
    }

    // --- gen=1..=COIN_GENERATIONS: spend child/grandchild/great-grandchild --
    //
    // A gen=N coin cannot exist on-chain until at least N blocks after
    // current_height (each ancestor must be spent in a separate block).
    // Therefore the earliest valid mine height for gen=N is
    // current_height + N + 1.  We shift the height offsets accordingly
    // so the grid covers heights that are actually reachable when the
    // CoinStateUpdate for the gen=N coin arrives.
    let mut ancestor = root_cat.clone();
    for gen in 1..=COIN_GENERATIONS {
        // Reward at the height the ANCESTOR is expected to be spent.
        let base_epoch = get_epoch(current_height + (gen as u32 - 1));
        let base_reward = get_reward(base_epoch);

        if ancestor.coin.amount < base_reward {
            if config.debug {
                println!(
                    "[debug] precompute_grid: gen={gen} coin amount ({}) < reward ({base_reward}), stopping lineage walk",
                    ancestor.coin.amount
                );
            }
            break;
        }

        let child_amount = ancestor.coin.amount - base_reward;
        let child_cat = ancestor.child(inner_puzzle_hash, child_amount);

        for &height_offset in &HEIGHT_OFFSETS {
            // Shift by gen: gen=1 → offsets 2,3,4; gen=2 → 3,4,5; etc.
            let target_height = current_height + height_offset + gen as u32;
            let user_height = target_height - 1;
            let epoch = get_epoch(target_height);
            let diff_bits = get_difficulty_bits(epoch);

            let cancel = Arc::new(AtomicBool::new(false));
            let grind_start = Instant::now();
            let nonce = match find_valid_nonce(
                &inner_puzzle_hash,
                pk_bytes,
                user_height,
                diff_bits,
                MAX_NONCE_ATTEMPTS,
                config.thread_count,
                cancel,
            ) {
                Some(n) => n,
                None => {
                    if config.debug {
                        println!(
                            "[debug] precompute_grid: gen={gen} h={target_height} user_h={user_height} — nonce not found, skipping"
                        );
                    }
                    continue;
                }
            };
            println!(
                "Found nonce {nonce} in {:.2?} (precomputed gen={gen} height={target_height}, user_height={user_height})",
                grind_start.elapsed()
            );

            let bundle = match build_mining_bundle(
                config,
                &child_cat,
                user_height,
                nonce,
                inner_puzzle_hash,
                pk_bytes,
                sk,
                fee_coins,
                fee_puzzlehash,
                synthetic_sk,
                synthetic_pk,
            ) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!(
                        "precompute_grid: gen={gen} h={target_height} user_h={user_height} bundle build error: {e}"
                    );
                    continue;
                }
            };

            let coin_id = child_cat.coin.coin_id();
            println!(
                "Precomputed bundle ready for height {target_height} (user_height={user_height}, nonce={nonce}, coin={}…, gen={gen})",
                &hex::encode(coin_id)
            );

            grid.push(PrecomputedBundle {
                target_height,
                target_coin_id: coin_id,
                bundle,
                nonce,
            });
        }

        // Advance: the child becomes the new ancestor for the next generation.
        // Use child_amount as the surviving amount after spending.
        ancestor = child_cat;
    }

    println!(
        "Bundle grid ready: {} entries ({} coin generations × {} heights, gen-shifted)",
        grid.len(),
        1 + COIN_GENERATIONS,
        HEIGHT_OFFSETS.len(),
    );
    if config.debug {
        for entry in &grid {
            println!(
                "[debug]   grid entry: coin={}… height={} nonce={}",
                &hex::encode(entry.target_coin_id),
                entry.target_height,
                entry.nonce,
            );
        }
    }
    grid
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
) -> Result<Option<Cat>> {
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

    bootstrap_cat_from_coin(clients, &cr.coin, inner_puzzle_hash).await
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
                                                    "Win CONFIRMED at height {}!",
                                                    cr.spent_block_index
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

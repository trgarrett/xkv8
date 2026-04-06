//! Main mining loops: polling-based and instant-react (Peer subscription).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chia_bls::{PublicKey, SecretKey};
use chia_protocol::{
    Bytes32, Coin, CoinSpend, CoinStateFilters, CoinStateUpdate,
    NewPeakWallet, ProtocolMessageTypes, SpendBundle,
};
use chia_puzzle_types::DeriveSynthetic;
use chia_puzzle_types::standard::StandardArgs;
use chia_traits::Streamable;
use chia_wallet_sdk::client::{
    PeerOptions, connect_peer, create_rustls_connector, load_ssl_cert,
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
                        eprintln!("Peer connection lost: {e}");
                        eprintln!("Reconnecting in 3 seconds…");
                        tokio::time::sleep(Duration::from_secs(3)).await;
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
                &hex::encode(full_cat_ph)[..16]
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

    // Skip if already submitted within validity window
    if let Some(&last_sub) = submitted_coins.get(&coin_id_key) {
        if mine_height < last_sub + 3 {
            if config.debug {
                println!(
                    "Height {mine_height}: coin {}… already submitted at height {last_sub}, waiting",
                    &hex::encode(coin_id_key)[..16]
                );
            }
            return Ok(());
        }
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

    // Grind for a valid nonce
    println!(
        "Grinding nonce at height {mine_height} (epoch={epoch}, reward={reward}, difficulty=2^{difficulty_bits})…"
    );
    let cancel = Arc::new(AtomicBool::new(false));
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
            eprintln!("Could not find valid nonce for height {mine_height}");
            return Ok(());
        }
    };
    println!("Found nonce {nonce} — building spend bundle…");

    // Get parent spend to reconstruct CAT lineage
    let target_cat = bootstrap_cat_from_coin(clients, &largest_cr.coin, inner_puzzle_hash).await?;

    let target_cat = match target_cat {
        Some(c) => c,
        None => {
            eprintln!("Could not reconstruct CAT lineage for coin");
            return Ok(());
        }
    };

    // Fetch fee coins if needed
    let fee_coins = if config.fee_mojos > 0 {
        fetch_fee_coins(clients, fee_puzzlehash, height).await
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

    let result = push_tx_to_all(clients, &bundle).await;
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
            "transport" => eprintln!("Failed to push tx (transport error): {:?}", result.error),
            "mempool_conflict" => eprintln!("Mempool conflict: {:?}", result.error),
            cat => eprintln!(
                "Failed to submit mining spend bundle: {:?} [{cat}]",
                result.error
            ),
        }
    }

    Ok(())
}

// ── Instant-react mining (LOCAL_FULL_NODE only) ────────────────────────

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

    let ssl_dir = config
        .chia_root
        .join("config")
        .join("ssl")
        .join("full_node");
    let cert_path = ssl_dir.join("private_full_node.crt");
    let key_path = ssl_dir.join("private_full_node.key");
    let cert = load_ssl_cert(
        cert_path.to_str().unwrap_or(""),
        key_path.to_str().unwrap_or(""),
    )?;
    let connector = create_rustls_connector(&cert)?;
    let options = PeerOptions::default();

    let (peer, mut receiver) =
        connect_peer(config.network_name.clone(), connector, socket_addr, options).await?;
    println!("Peer connected to {socket_addr}");

    // Bootstrap Cat from RPC
    let initial_cat =
        bootstrap_cat_from_rpc(clients, full_cat_ph, inner_puzzle_hash, height).await?;
    let initial_cat = initial_cat.context("Failed to bootstrap initial Cat object")?;
    println!(
        "Bootstrapped Cat: coin_id={}…, amount={}",
        &hex::encode(initial_cat.coin.coin_id())[..16],
        initial_cat.coin.amount
    );

    let mut submitted_coins: SubmittedCoins = HashMap::new();
    let mut current_cat: Option<Cat> = Some(initial_cat);
    let mut current_height = height;

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

    // Subscribe to lode coin puzzle hash
    let filters = CoinStateFilters::new(true, true, false, 0);
    let mut puzzle_hashes = vec![full_cat_ph];
    if config.fee_mojos > 0 {
        puzzle_hashes.push(fee_puzzlehash);
    }

    let _resp = peer
        .request_puzzle_state(
            puzzle_hashes.clone(),
            Some(height.saturating_sub(10)),
            header_hash,
            filters,
            true,
        )
        .await?;
    println!(
        "Subscribed to {} puzzle hash(es) (lode{})",
        puzzle_hashes.len(),
        if config.fee_mojos > 0 { " + fee" } else { "" }
    );

    // Start precomputing first bundle
    let mut precomputed_bundle: Option<PrecomputedBundle> = precompute_bundle(
        config,
        &current_cat,
        current_height,
        inner_puzzle_hash,
        pk_bytes,
        sk,
        fee_puzzlehash,
        synthetic_sk,
        synthetic_pk,
        &fee_coins,
    );

    println!("Instant-react mining active — waiting for block events…");
    println!();

    // Event loop
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(60), receiver.recv()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                anyhow::bail!("Peer disconnected");
            }
            Err(_) => continue, // Timeout, keep waiting
        };

        match msg.msg_type {
            ProtocolMessageTypes::CoinStateUpdate => {
                let update = CoinStateUpdate::from_bytes(&msg.data)?;
                let update_height = update.height;

                for coin_state in &update.items {
                    let coin = &coin_state.coin;

                    // ── Lode coin events ─────────────────────────
                    if coin.puzzle_hash == full_cat_ph {
                        // New lode coin confirmed
                        if coin_state.created_height.is_some()
                            && coin_state.spent_height.is_none()
                        {
                            println!(
                                "New lode coin confirmed at height {:?}: coin_id={}…, amount={}",
                                coin_state.created_height,
                                &hex::encode(coin.coin_id())[..16],
                                coin.amount
                            );

                            if let Some(ref pre) = precomputed_bundle {
                                if pre.target_coin_id == coin.coin_id() {
                                    // FIRE IMMEDIATELY
                                    println!(
                                        "Pushing PRECOMPUTED bundle for height {} (nonce={})",
                                        pre.target_height, pre.nonce
                                    );
                                    let result = push_tx_to_all(clients, &pre.bundle).await;
                                    if result.success {
                                        submitted_coins
                                            .insert(coin.coin_id(), pre.target_height);
                                        println!(
                                            "Submitted precomputed mining spend for height {}, Status={:?}",
                                            pre.target_height, result.status
                                        );
                                    } else {
                                        eprintln!(
                                            "Precomputed push failed: {:?} [{}]",
                                            result.error, result.error_category
                                        );
                                    }
                                } else {
                                    // Build fresh
                                    println!("No matching precomputed bundle — building fresh");
                                    precomputed_bundle = None;

                                    if let Some(ref cur_cat) = current_cat {
                                        let new_cat =
                                            cur_cat.child(inner_puzzle_hash, coin.amount);
                                        let cat_to_use =
                                            if new_cat.coin.coin_id() == coin.coin_id() {
                                                Some(new_cat)
                                            } else {
                                                bootstrap_cat_from_rpc(
                                                    clients,
                                                    full_cat_ph,
                                                    inner_puzzle_hash,
                                                    update_height,
                                                )
                                                .await
                                                .ok()
                                                .flatten()
                                            };

                                        if let Some(fresh_cat) = cat_to_use {
                                            let fresh_height = update_height + 1;
                                            let epoch = get_epoch(fresh_height);
                                            let reward_val = get_reward(epoch);
                                            let diff_bits = get_difficulty_bits(epoch);

                                            if coin.amount < reward_val {
                                                eprintln!(
                                                    "Lode coin amount ({}) < reward ({}), skipping",
                                                    coin.amount, reward_val
                                                );
                                                continue;
                                            }

                                            let cancel = Arc::new(AtomicBool::new(false));
                                            let iph = inner_puzzle_hash;
                                            let pkb = *pk_bytes;
                                            let cancel2 = cancel.clone();
                                            let tc = config.thread_count;
                                            let nonce = tokio::task::spawn_blocking(move || {
                                                find_valid_nonce(
                                                    &iph,
                                                    &pkb,
                                                    fresh_height,
                                                    diff_bits,
                                                    MAX_NONCE_ATTEMPTS,
                                                    tc,
                                                    cancel2,
                                                )
                                            })
                                            .await?;

                                            if let Some(nonce) = nonce {
                                                match build_mining_bundle(
                                                    config,
                                                    &fresh_cat,
                                                    fresh_height,
                                                    nonce,
                                                    inner_puzzle_hash,
                                                    pk_bytes,
                                                    sk,
                                                    &fee_coins,
                                                    fee_puzzlehash,
                                                    synthetic_sk,
                                                    synthetic_pk,
                                                ) {
                                                    Ok(bundle) => {
                                                        let result =
                                                            push_tx_to_all(clients, &bundle).await;
                                                        if result.success {
                                                            submitted_coins
                                                                .insert(coin.coin_id(), fresh_height);
                                                            println!("Submitted fresh mining spend for height {fresh_height}, Status={:?}", result.status);
                                                        } else {
                                                            eprintln!("Fresh push failed: {:?} [{}]", result.error, result.error_category);
                                                        }
                                                    }
                                                    Err(e) => {
                                                        eprintln!(
                                                            "Error building fresh bundle: {e}"
                                                        );
                                                    }
                                                }
                                            }
                                            current_cat = Some(fresh_cat);
                                        }
                                    }
                                }
                            }

                            // Update cat for next cycle
                            let epoch = get_epoch(current_height + 1);
                            let future_reward = get_reward(epoch);
                            if let Some(ref cur_cat) = current_cat {
                                if coin.amount >= future_reward {
                                    current_cat =
                                        Some(cur_cat.child(inner_puzzle_hash, coin.amount));
                                }
                            }
                            current_height = current_height.max(update_height);

                            // Precompute next bundle
                            precomputed_bundle = precompute_bundle(
                                config,
                                &current_cat,
                                current_height,
                                inner_puzzle_hash,
                                pk_bytes,
                                sk,
                                fee_puzzlehash,
                                synthetic_sk,
                                synthetic_pk,
                                &fee_coins,
                            );
                        }

                        // Lode coin spent
                        if coin_state.spent_height.is_some() {
                            if submitted_coins.remove(&coin.coin_id()).is_some() {
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
                    }

                    // ── Fee coin events ──────────────────────────
                    if config.fee_mojos > 0 && coin.puzzle_hash == fee_puzzlehash {
                        if coin_state.spent_height.is_some() {
                            fee_coins.retain(|c| c.coin_id() != coin.coin_id());
                        } else if coin_state.created_height.is_some() {
                            fee_coins.push(*coin);
                        }
                    }
                }
            }
            ProtocolMessageTypes::NewPeakWallet => {
                let peak = NewPeakWallet::from_bytes(&msg.data)?;
                let new_height = peak.height;
                if new_height != current_height {
                    if new_height % 100 == 0 {
                        println!("Height: {new_height}");
                    }
                    current_height = new_height;

                    if let Some(ref pre) = precomputed_bundle {
                        if pre.target_height != new_height + 1 {
                            println!(
                                "Height advanced to {new_height}, precomputed target {} is stale — recomputing",
                                pre.target_height
                            );
                            precomputed_bundle = precompute_bundle(
                                config,
                                &current_cat,
                                current_height,
                                inner_puzzle_hash,
                                pk_bytes,
                                sk,
                                fee_puzzlehash,
                                synthetic_sk,
                                synthetic_pk,
                                &fee_coins,
                            );
                        }
                    }

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
            }
            _ => {} // Ignore other message types
        }
    }
}

// ── Precomputed bundle ─────────────────────────────────────────────────

struct PrecomputedBundle {
    target_height: u32,
    target_coin_id: Bytes32,
    bundle: SpendBundle,
    nonce: u64,
}

#[allow(clippy::too_many_arguments)]
fn precompute_bundle(
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
) -> Option<PrecomputedBundle> {
    let cat = current_cat.as_ref()?;

    let current_epoch = get_epoch(current_height);
    let current_reward = get_reward(current_epoch);

    if cat.coin.amount < current_reward {
        eprintln!(
            "Lode coin amount ({}) insufficient for reward ({})",
            cat.coin.amount, current_reward
        );
        return None;
    }

    let child_amount = cat.coin.amount - current_reward;
    let future_height = current_height + 1;
    let future_epoch = get_epoch(future_height);
    let future_diff_bits = get_difficulty_bits(future_epoch);

    let future_cat = cat.child(inner_puzzle_hash, child_amount);

    let cancel = Arc::new(AtomicBool::new(false));
    let nonce = find_valid_nonce(
        &inner_puzzle_hash,
        pk_bytes,
        future_height,
        future_diff_bits,
        MAX_NONCE_ATTEMPTS,
        config.thread_count,
        cancel,
    )?;

    let bundle = build_mining_bundle(
        config,
        &future_cat,
        future_height,
        nonce,
        inner_puzzle_hash,
        pk_bytes,
        sk,
        fee_coins,
        fee_puzzlehash,
        synthetic_sk,
        synthetic_pk,
    )
    .ok()?;

    let coin_id = future_cat.coin.coin_id();
    println!(
        "Precomputed bundle ready for height {} (nonce={}, coin={}…)",
        future_height,
        nonce,
        &hex::encode(coin_id)[..16]
    );

    Some(PrecomputedBundle {
        target_height: future_height,
        target_coin_id: coin_id,
        bundle,
        nonce,
    })
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
                                &hex::encode(coin_id)[..16]
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

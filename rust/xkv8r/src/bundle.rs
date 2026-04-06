//! Spend bundle construction for mining (CAT spend + optional fee).

use anyhow::{Context, Result};
use chia_bls::{PublicKey, SecretKey, Signature, sign};
use chia_protocol::{Bytes32, Coin, SpendBundle};
use chia_wallet_sdk::driver::{
    Cat, CatSpend, Spend, SpendContext, StandardLayer,
};
use chia_wallet_sdk::signer::{AggSigConstants, RequiredSignature};
use chia_wallet_sdk::types::{Conditions, MAINNET_CONSTANTS, TESTNET11_CONSTANTS};
use chia_wallet_sdk::utils::select_coins;
use clvm_traits::ToClvm;
use clvmr::{Allocator, NodePtr};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::pow;
use crate::puzzle;

/// Build a complete mining spend bundle (CAT spend + optional fee).
pub fn build_mining_bundle(
    config: &Config,
    target_cat: &Cat,
    mine_height: u32,
    nonce: u64,
    inner_puzzle_hash: Bytes32,
    pk_bytes: &[u8],
    sk: &SecretKey,
    fee_coins: &[Coin],
    fee_puzzlehash: Bytes32,
    synthetic_sk: &SecretKey,
    synthetic_pk: &PublicKey,
) -> Result<SpendBundle> {
    let mut ctx = SpendContext::new();
    let coin = target_cat.coin;
    let coin_id = coin.coin_id();

    // Re-create the curried puzzle in this context's allocator
    let curried_ptr = puzzle::build_curried_puzzle_in_ctx(&mut ctx)?;

    // Build the inner puzzle solution:
    //   (my_amount my_inner_puzzlehash mine_height miner_pubkey target_puzzle_hash nonce)
    let my_amount_atom = (coin.amount as i64).to_clvm(&mut *ctx)?;
    let inner_ph_atom = ctx.new_atom(inner_puzzle_hash.as_ref())?;
    let mine_height_atom = (mine_height as i64).to_clvm(&mut *ctx)?;
    let pk_atom = ctx.new_atom(pk_bytes)?;
    let target_ph_atom = ctx.new_atom(config.target_puzzlehash.as_ref())?;
    let nonce_atom = (nonce as i64).to_clvm(&mut *ctx)?;

    // Build solution list: (my_amount inner_ph mine_height pk target_ph nonce)
    let nil = NodePtr::NIL;
    let sol = ctx.new_pair(nonce_atom, nil)?;
    let sol = ctx.new_pair(target_ph_atom, sol)?;
    let sol = ctx.new_pair(pk_atom, sol)?;
    let sol = ctx.new_pair(mine_height_atom, sol)?;
    let sol = ctx.new_pair(inner_ph_atom, sol)?;
    let sol = ctx.new_pair(my_amount_atom, sol)?;

    let inner_spend = Spend::new(curried_ptr, sol);
    let cat_spend = CatSpend::new(*target_cat, inner_spend);
    Cat::spend_all(&mut ctx, &[cat_spend])?;

    // Sign with AGG_SIG_ME
    let agg_sig_msg = pow::pow_sha256(&[
        config.target_puzzlehash.as_ref(),
        &pow::int_to_clvm_bytes(nonce as i64),
        &pow::int_to_clvm_bytes_u32(mine_height),
    ]);
    let mut full_msg = Vec::with_capacity(32 + 32 + 32);
    full_msg.extend_from_slice(&agg_sig_msg);
    full_msg.extend_from_slice(coin_id.as_ref());
    full_msg.extend_from_slice(config.genesis_challenge.as_ref());
    let sig = sign(sk, &full_msg);

    // Optional fee coin attachment
    let mut fee_sig = Signature::default();
    if config.fee_mojos > 0 && !fee_coins.is_empty() {
        match build_fee_spends(
            &mut ctx,
            config,
            fee_coins,
            fee_puzzlehash,
            synthetic_sk,
            synthetic_pk,
            &coin_id,
        ) {
            Ok(fsig) => {
                fee_sig = fsig;
            }
            Err(e) => {
                eprintln!("Error building fee spend: {e}");
            }
        }
    } else if config.fee_mojos > 0 {
        eprintln!(
            "Warning: FEE_MOJOS={} but no usable fee coins — submitting without fee",
            config.fee_mojos
        );
    }

    let coin_spends = ctx.take();
    let aggregated_sig = sig + &fee_sig;

    let bundle = SpendBundle::new(coin_spends, aggregated_sig);

    if config.debug {
        print_debug_bundle(&bundle);
    }

    Ok(bundle)
}

/// Build fee coin spends and return the aggregated fee signature.
fn build_fee_spends(
    ctx: &mut SpendContext,
    config: &Config,
    fee_coins: &[Coin],
    fee_puzzlehash: Bytes32,
    synthetic_sk: &SecretKey,
    synthetic_pk: &PublicKey,
    mining_coin_id: &Bytes32,
) -> Result<Signature> {
    let selected = select_coins(fee_coins.to_vec(), config.fee_mojos)
        .context("Fee coin selection failed")?;

    if selected.is_empty() {
        anyhow::bail!("No fee coins selected");
    }

    let total_in: u64 = selected.iter().map(|c| c.amount).sum();
    let change = total_in - config.fee_mojos;

    // announcement_id = sha256(mining_coin_id + b'$')
    let announcement_id: [u8; 32] = {
        let mut h = Sha256::new();
        h.update(mining_coin_id.as_ref());
        h.update(b"$");
        h.finalize().into()
    };

    let p2 = StandardLayer::new(*synthetic_pk);

    // First fee coin: assert announcement + reserve fee + optional change
    {
        let mut conditions = Conditions::new()
            .assert_coin_announcement(Bytes32::new(announcement_id))
            .reserve_fee(config.fee_mojos);

        if change > 0 {
            let hint = ctx.hint(fee_puzzlehash)?;
            conditions = conditions.create_coin(fee_puzzlehash, change, hint);
        }

        p2.spend(ctx, selected[0], conditions)?;
    }

    // Remaining fee coins: empty delegated spend
    for extra in &selected[1..] {
        p2.spend(ctx, *extra, Conditions::new())?;
    }

    // Sign each fee coin spend using RequiredSignature extraction
    let agg_sig_constants = AggSigConstants::from(if config.is_testnet {
        &*TESTNET11_CONSTANTS
    } else {
        &*MAINNET_CONSTANTS
    });

    let mut sigs = Vec::new();
    for cs in ctx.iter() {
        if selected.iter().any(|s| s.coin_id() == cs.coin.coin_id()) {
            let mut alloc = Allocator::new();
            let req_sigs =
                RequiredSignature::from_coin_spend(&mut alloc, cs, &agg_sig_constants)?;
            for req in req_sigs {
                match req {
                    RequiredSignature::Bls(bls_sig) => {
                        let msg = bls_sig.message();
                        sigs.push(sign(synthetic_sk, &msg));
                    }
                    _ => {}
                }
            }
        }
    }

    let total_fee_coins = selected.len();
    println!(
        "Attached fee of {} mojos ({} coin(s), change={})",
        config.fee_mojos, total_fee_coins, change
    );

    if sigs.is_empty() {
        Ok(Signature::default())
    } else {
        let mut agg = sigs[0].clone();
        for s in &sigs[1..] {
            agg += s;
        }
        Ok(agg)
    }
}

fn print_debug_bundle(bundle: &SpendBundle) {
    let coin_spends_json: Vec<serde_json::Value> = bundle
        .coin_spends
        .iter()
        .map(|cs| {
            serde_json::json!({
                "coin": {
                    "parent_coin_info": hex::encode(cs.coin.parent_coin_info),
                    "puzzle_hash": hex::encode(cs.coin.puzzle_hash),
                    "amount": cs.coin.amount,
                },
                "puzzle_reveal": hex::encode(cs.puzzle_reveal.as_ref()),
                "solution": hex::encode(cs.solution.as_ref()),
            })
        })
        .collect();

    let bundle_json = serde_json::json!({
        "coin_spends": coin_spends_json,
        "aggregated_signature": hex::encode(bundle.aggregated_signature.to_bytes()),
    });

    println!("[DEBUG] Spend bundle JSON:");
    println!("{}", serde_json::to_string_pretty(&bundle_json).unwrap());
}

//! Puzzle compilation, currying, and related helpers.

use anyhow::Result;
use chia_protocol::Bytes32;
use chia_puzzle_types::cat::CatArgs;
use chia_wallet_sdk::types::Mod;
use clvm_traits::ToClvm;
use clvm_utils::{CurriedProgram, TreeHash, tree_hash};
use clvmr::{
    Allocator, NodePtr,
    serde::node_from_bytes,
};

use crate::config::{BASE_DIFFICULTY_BITS, BASE_REWARD, CAT_TAIL_HASH, EPOCH_LENGTH, GENESIS_HEIGHT};

/// Compiled puzzle hex (puzzle.clsp) — public so bundle.rs can re-use it.
pub const PUZZLE_HEX_STR: &str = concat!(
    "ff02ffff01ff02ff7effff04ff02ffff04ff8202ffffff04ffff02ff52ffff04ff02ffff04",
    "ff0bffff04ff17ffff04ff8205ffff808080808080ffff04ff8205ffffff04ff820bffffff",
    "04ff2fffff04ff8217ffffff04ff822fffffff04ff825fffffff04ffff02ff56ffff04ff02",
    "ffff04ff81bfffff04ffff02ff26ffff04ff02ffff04ff820bffffff04ff2fffff04ff5fff",
    "808080808080ff8080808080ffff04ffff02ff7affff04ff02ffff04ff82017fffff04ffff",
    "02ff26ffff04ff02ffff04ff820bffffff04ff2fffff04ff5fff808080808080ff80808080",
    "80ff80808080808080808080808080ffff04ffff01ffffffff3257ff53ff5249ffff48ff33",
    "3cff01ff0102ffffffff02ffff03ff05ffff01ff0bff8201f2ffff02ff76ffff04ff02ffff",
    "04ff09ffff04ffff02ff22ffff04ff02ffff04ff0dff80808080ff808080808080ffff0182",
    "01b280ff0180ffff02ff2affff04ff02ffff04ff05ffff04ffff02ff5effff04ff02ffff04",
    "ff05ff80808080ffff04ffff02ff5effff04ff02ffff04ff0bff80808080ffff04ff17ff80",
    "808080808080ffffa04bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce2",
    "3c7785459aa09dcf97a184f32623d11a73124ceb99a5709b083721e878a16d78f596718b",
    "a7b2ffa102a12871fee210fb8619291eaea194581cbd2531e4b23759d225f6806923f6322",
    "2a102a8d5dd63fba471ebcb1f3e8f7c1e1879b7152a6e7298a91ce119a63400ade7c5fff",
    "f0bff820172ffff02ff76ffff04ff02ffff04ff05ffff04ffff02ff22ffff04ff02ffff04",
    "ff07ff80808080ff808080808080ffff04ffff04ff78ffff04ff05ff808080ffff04ffff04",
    "ff24ffff04ff0bff808080ffff04ffff04ff58ffff01ff018080ffff04ffff04ff28ffff04",
    "ff2fff808080ffff04ffff04ff30ffff04ffff10ff2fffff010380ff808080ffff04ffff04",
    "ff20ffff04ff5fffff04ffff0bff81bfff82017fff2f80ff80808080ffff04ffff04ff5cff",
    "ff04ff5fff808080ffff04ffff04ff54ffff04ff81bfffff04ff8202ffffff04ffff04ff81",
    "bfff8080ff8080808080ffff04ffff04ff54ffff04ff17ffff04ffff11ff05ff8202ff80ff",
    "ff04ffff04ff17ff8080ff8080808080ffff04ffff04ff74ffff01ff248080ff8080808080",
    "808080808080ff16ff05ffff11ff80ff0b8080ffffff02ffff03ffff15ffff05ffff14ffff",
    "11ff05ff0b80ff178080ffff010380ffff01ff0103ffff01ff05ffff14ffff11ff05ff0b80",
    "ff17808080ff0180ffff16ff05ffff11ff80ff0b8080ff0bff7cffff0bff7cff8201b2ff05",
    "80ffff0bff7cff0bff8201328080ffff02ffff03ffff15ff05ff8080ffff01ff15ff0bff05",
    "80ff8080ff0180ffff02ffff03ffff07ff0580ffff01ff0bff7cffff02ff5effff04ff02ff",
    "ff04ff09ff80808080ffff02ff5effff04ff02ffff04ff0dff8080808080ffff01ff0bff2c",
    "ff058080ff0180ff02ffff03ffff15ff2fff5f80ffff01ff02ffff03ffff02ff2effff04ff",
    "02ffff04ffff0bff17ff81bfff2fff8202ff80ffff04ff820bffff8080808080ffff01ff02",
    "ffff03ffff20ffff15ff8205ffff058080ffff01ff02ff5affff04ff02ffff04ff05ffff04",
    "ff0bffff04ff17ffff04ff2fffff04ff81bfffff04ff82017fffff04ff8202ffffff04ff82",
    "05ffff8080808080808080808080ffff01ff088080ff0180ffff01ff088080ff0180ffff01",
    "ff088080ff0180ff018080"
);


/// Curry the compiled puzzle with the static mining parameters.
///
/// Returns the inner puzzle hash (tree hash of the curried program).
/// The actual puzzle NodePtr must be re-created per SpendContext since
/// NodePtrs are allocator-local.
pub fn build_curried_puzzle_hash() -> Result<Bytes32> {
    let mut allocator = Allocator::new();
    let puzzle_bytes = hex::decode(PUZZLE_HEX_STR)?;
    let mod_ptr = node_from_bytes(&mut allocator, &puzzle_bytes)?;
    let mod_hash = tree_hash(&allocator, mod_ptr);

    let cat_mod_hash = <CatArgs<NodePtr> as Mod>::mod_hash();

    // Build curry arguments
    let mod_hash_bytes: [u8; 32] = mod_hash.into();
    let mod_hash_atom = allocator.new_atom(&mod_hash_bytes)?;
    let cat_mod_hash_bytes: [u8; 32] = cat_mod_hash.into();
    let cat_mod_hash_atom = allocator.new_atom(&cat_mod_hash_bytes)?;
    let tail_hash_atom = allocator.new_atom(CAT_TAIL_HASH.as_ref())?;
    let genesis_height_atom = (GENESIS_HEIGHT as i64).to_clvm(&mut allocator)?;
    let epoch_length_atom = (EPOCH_LENGTH as i64).to_clvm(&mut allocator)?;
    let base_reward_atom = (BASE_REWARD as i64).to_clvm(&mut allocator)?;

    // BASE_DIFFICULTY = 2^238
    let base_difficulty = num_bigint::BigUint::from(1u32) << BASE_DIFFICULTY_BITS;
    let diff_bytes = base_difficulty.to_bytes_be();
    let base_difficulty_atom = allocator.new_atom(&diff_bytes)?;

    // Build curry args: (arg1 arg2 ... argN . 1)
    let one = allocator.one();
    let args = allocator.new_pair(base_difficulty_atom, one)?;
    let args = allocator.new_pair(base_reward_atom, args)?;
    let args = allocator.new_pair(epoch_length_atom, args)?;
    let args = allocator.new_pair(genesis_height_atom, args)?;
    let args = allocator.new_pair(tail_hash_atom, args)?;
    let args = allocator.new_pair(cat_mod_hash_atom, args)?;
    let args = allocator.new_pair(mod_hash_atom, args)?;

    let curried_ptr = CurriedProgram {
        program: mod_ptr,
        args,
    }
    .to_clvm(&mut allocator)?;

    let inner_puzzle_hash: Bytes32 = tree_hash(&allocator, curried_ptr).into();
    Ok(inner_puzzle_hash)
}

/// Construct a curried puzzle NodePtr inside the given allocator/SpendContext.
pub fn build_curried_puzzle_in_ctx(ctx: &mut SpendContext) -> Result<NodePtr> {
    let puzzle_bytes = hex::decode(PUZZLE_HEX_STR)?;
    let mod_ptr = clvmr::serde::node_from_bytes(&mut **ctx, &puzzle_bytes)?;
    let mod_hash = ctx.tree_hash(mod_ptr);

    let cat_mod_hash = <CatArgs<NodePtr> as Mod>::mod_hash();

    let mod_hash_bytes: [u8; 32] = mod_hash.into();
    let mod_hash_atom = ctx.new_atom(&mod_hash_bytes)?;
    let cat_mod_hash_bytes: [u8; 32] = cat_mod_hash.into();
    let cat_mod_hash_atom = ctx.new_atom(&cat_mod_hash_bytes)?;
    let tail_hash_atom = ctx.new_atom(CAT_TAIL_HASH.as_ref())?;
    let genesis_height_atom = (GENESIS_HEIGHT as i64).to_clvm(&mut **ctx)?;
    let epoch_length_atom = (EPOCH_LENGTH as i64).to_clvm(&mut **ctx)?;
    let base_reward_atom = (BASE_REWARD as i64).to_clvm(&mut **ctx)?;

    let base_difficulty = num_bigint::BigUint::from(1u32) << BASE_DIFFICULTY_BITS;
    let diff_bytes = base_difficulty.to_bytes_be();
    let base_difficulty_atom = ctx.new_atom(&diff_bytes)?;

    let one = ctx.one();
    let args = ctx.new_pair(base_difficulty_atom, one)?;
    let args = ctx.new_pair(base_reward_atom, args)?;
    let args = ctx.new_pair(epoch_length_atom, args)?;
    let args = ctx.new_pair(genesis_height_atom, args)?;
    let args = ctx.new_pair(tail_hash_atom, args)?;
    let args = ctx.new_pair(cat_mod_hash_atom, args)?;
    let args = ctx.new_pair(mod_hash_atom, args)?;

    let curried_ptr = CurriedProgram {
        program: mod_ptr,
        args,
    }
    .to_clvm(&mut **ctx)?;

    Ok(curried_ptr)
}

use chia_wallet_sdk::driver::SpendContext;

/// Compute the full CAT puzzle hash for the given inner puzzle hash.
pub fn full_cat_puzzlehash(inner_puzzle_hash: Bytes32) -> Bytes32 {
    CatArgs::curry_tree_hash(CAT_TAIL_HASH, TreeHash::from(inner_puzzle_hash)).into()
}

/// Get the epoch number for a given height (capped at 3).
pub fn get_epoch(user_height: u32) -> u32 {
    let raw = (user_height.saturating_sub(GENESIS_HEIGHT)) / EPOCH_LENGTH;
    raw.min(3)
}

/// Get the reward for a given epoch (BASE_REWARD >> epoch).
pub fn get_reward(epoch: u32) -> u64 {
    BASE_REWARD >> epoch
}

/// Get the difficulty target bits for a given epoch (238 - epoch).
pub fn get_difficulty_bits(epoch: u32) -> u32 {
    BASE_DIFFICULTY_BITS - epoch
}

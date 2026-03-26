"""Diagnostic: compute all puzzle hash components and cross-reference with on-chain data."""

import sys, os
sys.path.insert(0, os.path.dirname(__file__))

from clvm_tools_rs import compile as compile_clsp
from chia_wallet_sdk import Clvm, Constants, cat_puzzle_hash, tree_hash_atom, tree_hash_pair, curry_tree_hash

# ── On-chain known values ────────────────────────────────────────────
ONCHAIN_PUZZLE_HASH = bytes.fromhex("498d2c5438b8e051ac9a03886a7d6769000061e2f4401670a775f4e3197157e5")

CAT_TAIL_HASH = bytes.fromhex(
    "c1a98dc2100e94acbdbb2af0e264eedd85703fbe70cbcd73910e85ed01ca163e"
)
GENESIS_HEIGHT = 3897519
EPOCH_LENGTH = 1_120_000
BASE_REWARD = 10_000
BASE_DIFFICULTY = 2**238

# ── Compile puzzle ───────────────────────────────────────────────────
def compile_puzzle():
    source = open("clsp/puzzle.clsp").read()
    return compile_clsp(source, ["clsp/include/"])

clvm = Clvm()

# ── Get CAT mod hash from Constants ─────────────────────────────────
cat_mod_hash_from_constants = Constants.cat_puzzle_hash()
print(f"Constants.cat_puzzle_hash():  {cat_mod_hash_from_constants.hex()}")

# ── Get CAT mod hash from actual puzzle program ─────────────────────
cat_puzzle_program = clvm.cat_puzzle()
cat_mod_hash_from_program = cat_puzzle_program.tree_hash()
print(f"clvm.cat_puzzle().tree_hash(): {cat_mod_hash_from_program.hex()}")
print(f"Match: {cat_mod_hash_from_constants == cat_mod_hash_from_program}")
print()

# ── Well-known CAT v2 mod hash ──────────────────────────────────────
WELL_KNOWN_CAT_MOD_HASH = bytes.fromhex("72dec062874cd4d3aab892a0906688a1ae412b0109982e1797a170add88bdcdc")
print(f"Well-known CAT v2 mod hash:   {WELL_KNOWN_CAT_MOD_HASH.hex()}")
print(f"Constants matches well-known: {cat_mod_hash_from_constants == WELL_KNOWN_CAT_MOD_HASH}")
print()

# ── Compile and curry the inner puzzle ───────────────────────────────
hex_str = compile_puzzle()
mod = clvm.deserialize(bytes.fromhex(hex_str))
mod_hash = mod.tree_hash()
print(f"Uncurried mod hash:           {mod_hash.hex()}")

# Curry with Constants.cat_puzzle_hash() (what miner does)
curried = mod.curry([
    clvm.atom(mod_hash),
    clvm.atom(cat_mod_hash_from_constants),
    clvm.atom(CAT_TAIL_HASH),
    clvm.int(GENESIS_HEIGHT),
    clvm.int(EPOCH_LENGTH),
    clvm.int(BASE_REWARD),
    clvm.int(BASE_DIFFICULTY),
])
inner_puzzle_hash = curried.tree_hash()
print(f"Inner puzzle hash (curried):  {inner_puzzle_hash.hex()}")
print()

# ── Compute full CAT puzzle hash via SDK ─────────────────────────────
sdk_cat_ph = cat_puzzle_hash(CAT_TAIL_HASH, inner_puzzle_hash)
print(f"SDK cat_puzzle_hash():        {sdk_cat_ph.hex()}")
print(f"Matches on-chain:             {sdk_cat_ph == ONCHAIN_PUZZLE_HASH}")
print()

# ── Manually compute what the CLSP does ──────────────────────────────
# CLSP: curry_hashes(CAT_MOD_HASH, sha256tree1(CAT_MOD_HASH), sha256tree1(CAT_TAIL_HASH), inner_puzzle_hash)
# curry_tree_hash(mod_hash, [param_tree_hashes...])
th_cat_mod = tree_hash_atom(cat_mod_hash_from_constants)
th_tail = tree_hash_atom(CAT_TAIL_HASH)
print(f"tree_hash_atom(CAT_MOD_HASH): {th_cat_mod.hex()}")
print(f"tree_hash_atom(CAT_TAIL):     {th_tail.hex()}")
print(f"inner_puzzle_hash (as-is):    {inner_puzzle_hash.hex()}")

manual_cat_ph = curry_tree_hash(cat_mod_hash_from_constants, [th_cat_mod, th_tail, inner_puzzle_hash])
print(f"Manual curry_tree_hash():     {manual_cat_ph.hex()}")
print(f"Matches SDK:                  {manual_cat_ph == sdk_cat_ph}")
print(f"Matches on-chain:             {manual_cat_ph == ONCHAIN_PUZZLE_HASH}")
print()

# ── Try with well-known CAT mod hash if different ────────────────────
if cat_mod_hash_from_constants != WELL_KNOWN_CAT_MOD_HASH:
    curried_wk = mod.curry([
        clvm.atom(mod_hash),
        clvm.atom(WELL_KNOWN_CAT_MOD_HASH),
        clvm.atom(CAT_TAIL_HASH),
        clvm.int(GENESIS_HEIGHT),
        clvm.int(EPOCH_LENGTH),
        clvm.int(BASE_REWARD),
        clvm.int(BASE_DIFFICULTY),
    ])
    inner_ph_wk = curried_wk.tree_hash()
    sdk_cat_ph_wk = cat_puzzle_hash(CAT_TAIL_HASH, inner_ph_wk)
    th_cat_mod_wk = tree_hash_atom(WELL_KNOWN_CAT_MOD_HASH)
    manual_cat_ph_wk = curry_tree_hash(WELL_KNOWN_CAT_MOD_HASH, [th_cat_mod_wk, tree_hash_atom(CAT_TAIL_HASH), inner_ph_wk])
    print(f"--- With well-known CAT mod hash ---")
    print(f"Inner puzzle hash:            {inner_ph_wk.hex()}")
    print(f"SDK cat_puzzle_hash():        {sdk_cat_ph_wk.hex()}")
    print(f"Manual curry_tree_hash():     {manual_cat_ph_wk.hex()}")
    print(f"SDK matches on-chain:         {sdk_cat_ph_wk == ONCHAIN_PUZZLE_HASH}")
    print(f"Manual matches on-chain:      {manual_cat_ph_wk == ONCHAIN_PUZZLE_HASH}")
    print()

print("=== SUMMARY ===")
print(f"On-chain puzzle hash:         {ONCHAIN_PUZZLE_HASH.hex()}")
print(f"SDK computed:                 {sdk_cat_ph.hex()}")
print(f"CLSP would compute (manual):  {manual_cat_ph.hex()}")
if sdk_cat_ph == ONCHAIN_PUZZLE_HASH and manual_cat_ph == ONCHAIN_PUZZLE_HASH:
    print("ALL MATCH - issue may be elsewhere (solution params, CAT layer interaction)")
elif sdk_cat_ph == ONCHAIN_PUZZLE_HASH and manual_cat_ph != ONCHAIN_PUZZLE_HASH:
    print("MISMATCH: CLSP computation differs from SDK! CAT_MOD_HASH curried value is likely wrong.")
elif sdk_cat_ph != ONCHAIN_PUZZLE_HASH:
    print("SDK doesn't match on-chain - inner puzzle hash may be wrong (different compilation?)")
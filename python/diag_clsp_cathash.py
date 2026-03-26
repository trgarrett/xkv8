"""Check what the CLSP cat_puzzle_hash actually computes vs what SDK expects."""
import sys, os
sys.path.insert(0, os.path.dirname(__file__))

from clvm_tools_rs import compile as compile_clsp
from chia_wallet_sdk import Clvm, Constants, cat_puzzle_hash, tree_hash_atom, curry_tree_hash

CAT_TAIL_HASH = bytes.fromhex("c1a98dc2100e94acbdbb2af0e264eedd85703fbe70cbcd73910e85ed01ca163e")
GENESIS_HEIGHT = 3897519
EPOCH_LENGTH = 1_120_000
BASE_REWARD = 10_000
BASE_DIFFICULTY = 2**238

clvm = Clvm()
cat_mod_hash = Constants.cat_puzzle_hash()

# Compile a tiny CLSP that just computes cat_puzzle_hash and returns it
# We'll use the actual puzzle but extract just the hash computation
hex_str = compile_clsp(open("clsp/puzzle.clsp").read(), ["clsp/include/"])
mod = clvm.deserialize(bytes.fromhex(hex_str))
mod_hash = mod.tree_hash()

curried = mod.curry([
    clvm.atom(mod_hash),
    clvm.atom(cat_mod_hash),
    clvm.atom(CAT_TAIL_HASH),
    clvm.int(GENESIS_HEIGHT),
    clvm.int(EPOCH_LENGTH),
    clvm.int(BASE_REWARD),
    clvm.int(BASE_DIFFICULTY),
])
inner_puzzle_hash = curried.tree_hash()

# The CLSP asserted e34adcf3...
# Let's figure out what curry_hashes produces with the well-known CAT mod hash
WELL_KNOWN = bytes.fromhex("72dec062874cd4d3aab892a0906688a1ae412b0109982e1797a170add88bdcdc")

# Test: what if the CLSP curry_hashes is using the well-known hash?
wk_result = curry_tree_hash(WELL_KNOWN, [
    tree_hash_atom(WELL_KNOWN),
    tree_hash_atom(CAT_TAIL_HASH),
    inner_puzzle_hash
])
print(f"curry_tree_hash with well-known CAT mod: {wk_result.hex()}")

# Test: what does the SDK cat_puzzle_hash produce?
sdk_result = cat_puzzle_hash(CAT_TAIL_HASH, inner_puzzle_hash)
print(f"SDK cat_puzzle_hash:                      {sdk_result.hex()}")

# Test: curry_tree_hash with Constants hash
const_result = curry_tree_hash(cat_mod_hash, [
    tree_hash_atom(cat_mod_hash),
    tree_hash_atom(CAT_TAIL_HASH),
    inner_puzzle_hash
])
print(f"curry_tree_hash with Constants hash:      {const_result.hex()}")

# The CLSP produced e34adcf3...
clsp_produced = bytes.fromhex("e34adcf3e9fb2e6c2d4297bba036ceb47df6681d6c40d7547f82dab2a55ba020")
print(f"CLSP actually produced:                   {clsp_produced.hex()}")
print()
print(f"CLSP matches well-known curry:  {clsp_produced == wk_result}")
print(f"CLSP matches Constants curry:   {clsp_produced == const_result}")
print(f"CLSP matches SDK:               {clsp_produced == sdk_result}")

# What if the CLSP is using the well-known hash because that's what was
# compiled into the puzzle constants? The curry.clib has hardcoded constants.
# But the CAT_MOD_HASH is curried in, not hardcoded in curry.clib.
# 
# Wait - maybe the issue is that the CLSP curry_hashes uses sha256tree1
# on the inner_puzzle_hash, treating it as an atom rather than a tree hash.
# Let's check: sha256tree1 of an atom = sha256(1, atom)
import hashlib
sha256_1_inner = hashlib.sha256(b'\x01' + inner_puzzle_hash).digest()
print(f"\nsha256tree1(inner_puzzle_hash): {sha256_1_inner.hex()}")

# What if CLSP double-hashes the inner puzzle hash?
double_hash_result = curry_tree_hash(cat_mod_hash, [
    tree_hash_atom(cat_mod_hash),
    tree_hash_atom(CAT_TAIL_HASH),
    tree_hash_atom(inner_puzzle_hash)  # double-hashed!
])
print(f"curry_tree_hash with double-hashed inner: {double_hash_result.hex()}")
print(f"Matches CLSP output:                      {double_hash_result == clsp_produced}")
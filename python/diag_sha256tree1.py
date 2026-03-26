"""Compare CLSP sha256tree1 vs SDK tree_hash_atom to find the discrepancy."""
import sys, os, hashlib
sys.path.insert(0, os.path.dirname(__file__))

from chia_wallet_sdk import Clvm, Constants, tree_hash_atom, curry_tree_hash

CAT_MOD_HASH = Constants.cat_puzzle_hash()

# SDK tree_hash_atom
sdk_tha = tree_hash_atom(CAT_MOD_HASH)
print(f"SDK tree_hash_atom(CAT_MOD_HASH): {sdk_tha.hex()}")

# Standard tree hash of atom: sha256(0x01 || atom)
std_hash = hashlib.sha256(b'\x01' + CAT_MOD_HASH).digest()
print(f"sha256(0x01 || CAT_MOD_HASH):     {std_hash.hex()}")
print(f"SDK matches standard:              {sdk_tha == std_hash}")

# CLSP sha256tree1 of atom: sha256(sha256(0x01) || atom)
sha256_one = hashlib.sha256(b'\x01').digest()
print(f"\nsha256(0x01) = {sha256_one.hex()}")
clsp_hash = hashlib.sha256(sha256_one + CAT_MOD_HASH).digest()
print(f"sha256(sha256(0x01) || CAT_MOD_HASH): {clsp_hash.hex()}")
print(f"SDK matches CLSP sha256tree1:          {sdk_tha == clsp_hash}")

# Now test: what does curry_tree_hash expect?
# If tree_hash_atom = sha256(0x01 || atom), then curry_tree_hash uses standard hashes
# If tree_hash_atom = sha256(sha256(0x01) || atom), then curry_tree_hash uses CLSP-style hashes

# Let's also check what the Clvm.tree_hash() produces for a simple atom
clvm = Clvm()
atom_prog = clvm.atom(CAT_MOD_HASH)
prog_tree_hash = atom_prog.tree_hash()
print(f"\nClvm.atom(CAT_MOD_HASH).tree_hash(): {prog_tree_hash.hex()}")
print(f"Matches standard sha256(0x01||atom): {prog_tree_hash == std_hash}")
print(f"Matches CLSP sha256tree1:            {prog_tree_hash == clsp_hash}")

# Now let's actually run sha256tree1 in CLVM to see what it produces
# We'll compile a tiny program that computes sha256tree1
from clvm_tools_rs import compile as compile_clsp

# Compile a program that computes sha256tree1 of its argument
test_src = """
(mod (X)
  (include curry.clib)
  (sha256tree1 X)
)
"""
test_hex = compile_clsp(test_src, ["clsp/include/"])
test_mod = clvm.deserialize(bytes.fromhex(test_hex))

# Run it with CAT_MOD_HASH as input
sol = clvm.parse(f"(0x{CAT_MOD_HASH.hex()})")
result = test_mod.run(sol, 1_000_000, False)
clsp_sha256tree1_result = result.value.to_atom()
print(f"\nCLSP sha256tree1(CAT_MOD_HASH):      {clsp_sha256tree1_result.hex()}")
print(f"Matches SDK tree_hash_atom:           {clsp_sha256tree1_result == sdk_tha}")
print(f"Matches standard sha256(0x01||atom):  {clsp_sha256tree1_result == std_hash}")
print(f"Matches sha256(sha256(0x01)||atom):   {clsp_sha256tree1_result == clsp_hash}")

# Now let's run the full curry_hashes in CLVM
test_src2 = """
(mod (MOD_HASH P1 P2 P3)
  (include curry.clib)
  (curry_hashes MOD_HASH
    (sha256tree1 P1)
    (sha256tree1 P2)
    P3
  )
)
"""
test_hex2 = compile_clsp(test_src2, ["clsp/include/"])
test_mod2 = clvm.deserialize(bytes.fromhex(test_hex2))

inner_ph = bytes.fromhex("33d7fb4ae438ca6a09c425d0da63e43b240cacf76315b8002295087d1292bbc9")
CAT_TAIL = bytes.fromhex("c1a98dc2100e94acbdbb2af0e264eedd85703fbe70cbcd73910e85ed01ca163e")

sol2 = clvm.parse(f"(0x{CAT_MOD_HASH.hex()} 0x{CAT_MOD_HASH.hex()} 0x{CAT_TAIL.hex()} 0x{inner_ph.hex()})")
result2 = test_mod2.run(sol2, 10_000_000, False)
clsp_curry_result = result2.value.to_atom()
print(f"\nCLSP curry_hashes result:             {clsp_curry_result.hex()}")

sdk_curry_result = curry_tree_hash(CAT_MOD_HASH, [tree_hash_atom(CAT_MOD_HASH), tree_hash_atom(CAT_TAIL), inner_ph])
print(f"SDK curry_tree_hash result:           {sdk_curry_result.hex()}")
print(f"Match:                                {clsp_curry_result == sdk_curry_result}")

# What if we use CLSP sha256tree1 values in SDK curry_tree_hash?
# First get CLSP sha256tree1 for CAT_TAIL
sol_tail = clvm.parse(f"(0x{CAT_TAIL.hex()})")
result_tail = test_mod.run(sol_tail, 1_000_000, False)
clsp_sha256tree1_tail = result_tail.value.to_atom()

sdk_with_clsp_hashes = curry_tree_hash(CAT_MOD_HASH, [clsp_sha256tree1_result, clsp_sha256tree1_tail, inner_ph])
print(f"\nSDK curry_tree_hash with CLSP hashes: {sdk_with_clsp_hashes.hex()}")
print(f"Matches CLSP curry_hashes:            {sdk_with_clsp_hashes == clsp_curry_result}")
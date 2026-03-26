"""Diagnostic: actually run the curried puzzle and check ASSERT_MY_PUZZLEHASH output."""

import sys, os, hashlib
sys.path.insert(0, os.path.dirname(__file__))

from clvm_tools_rs import compile as compile_clsp
from chia_wallet_sdk import Clvm, Constants, cat_puzzle_hash, tree_hash_atom, curry_tree_hash

ONCHAIN_PUZZLE_HASH = bytes.fromhex("498d2c5438b8e051ac9a03886a7d6769000061e2f4401670a775f4e3197157e5")

CAT_TAIL_HASH = bytes.fromhex(
    "c1a98dc2100e94acbdbb2af0e264eedd85703fbe70cbcd73910e85ed01ca163e"
)
GENESIS_HEIGHT = 3897519
EPOCH_LENGTH = 1_120_000
BASE_REWARD = 10_000
BASE_DIFFICULTY = 2**238

def compile_puzzle():
    source = open("clsp/puzzle.clsp").read()
    return compile_clsp(source, ["clsp/include/"])

def int_to_clvm_bytes(n):
    if n == 0:
        return b""
    byte_len = (n.bit_length() + 8) // 8
    return n.to_bytes(byte_len, "big", signed=True)

def find_valid_nonce(puzzle_hash, miner_pubkey, user_height, difficulty, max_attempts=5_000_000):
    h_bytes = int_to_clvm_bytes(user_height)
    for nonce in range(max_attempts):
        n_bytes = int_to_clvm_bytes(nonce)
        digest = hashlib.sha256(puzzle_hash + miner_pubkey + h_bytes + n_bytes).digest()
        pow_int = int.from_bytes(digest, "big")
        if pow_int > 0 and difficulty > pow_int:
            return nonce
    return None

clvm = Clvm()

cat_mod_hash = Constants.cat_puzzle_hash()
print(f"CAT mod hash: {cat_mod_hash.hex()}")

hex_str = compile_puzzle()
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
full_cat_ph = cat_puzzle_hash(CAT_TAIL_HASH, inner_puzzle_hash)

print(f"Inner puzzle hash: {inner_puzzle_hash.hex()}")
print(f"Full CAT puzzle hash: {full_cat_ph.hex()}")
print(f"On-chain match: {full_cat_ph == ONCHAIN_PUZZLE_HASH}")
print()

# Generate a fake miner pubkey (48 bytes for BLS)
miner_pubkey = b'\xab' * 48
target_puzzle_hash = b'\xcd' * 32
my_amount = 21_000_000_000
user_height = GENESIS_HEIGHT + 10

epoch = min((user_height - GENESIS_HEIGHT) // EPOCH_LENGTH, 3)
reward = BASE_REWARD >> epoch
difficulty = BASE_DIFFICULTY >> epoch

print(f"Epoch: {epoch}, Reward: {reward}, Difficulty: 2^{difficulty.bit_length()-1}")

nonce = find_valid_nonce(inner_puzzle_hash, miner_pubkey, user_height, difficulty)
print(f"Found nonce: {nonce}")

if nonce is None:
    print("Could not find nonce!")
    sys.exit(1)

# Build solution
solution_str = (
    f"({my_amount} "
    f"0x{inner_puzzle_hash.hex()} "
    f"{user_height} "
    f"0x{miner_pubkey.hex()} "
    f"0x{target_puzzle_hash.hex()} "
    f"{nonce})"
)
print(f"Solution: {solution_str[:120]}...")

solution = clvm.parse(solution_str)

# Run the puzzle
try:
    output = curried.run(solution, 11_000_000_000, False)
    conditions = output.value.to_list()
    print(f"\nPuzzle ran successfully! {len(conditions)} conditions produced.")
    
    for i, cond in enumerate(conditions):
        pair = cond.to_pair()
        opcode = pair.first.to_int()
        
        if opcode == 72:  # ASSERT_MY_PUZZLEHASH
            asserted_ph = pair.rest.first().to_atom()
            print(f"\n  Condition {i}: ASSERT_MY_PUZZLEHASH")
            print(f"    Asserted:  {asserted_ph.hex()}")
            print(f"    On-chain:  {ONCHAIN_PUZZLE_HASH.hex()}")
            print(f"    Full CAT:  {full_cat_ph.hex()}")
            print(f"    Match on-chain: {asserted_ph == ONCHAIN_PUZZLE_HASH}")
            print(f"    Match full CAT: {asserted_ph == full_cat_ph}")
            print(f"    Match inner:    {asserted_ph == inner_puzzle_hash}")
        elif opcode == 73:  # ASSERT_MY_AMOUNT
            amt = pair.rest.first().to_int()
            print(f"  Condition {i}: ASSERT_MY_AMOUNT = {amt}")
        elif opcode == 51:  # CREATE_COIN
            ph = pair.rest.first().to_atom()
            amt_prog = pair.rest.rest()
            amt = amt_prog.first().to_int()
            print(f"  Condition {i}: CREATE_COIN ph={ph.hex()[:16]}... amt={amt}")
        else:
            print(f"  Condition {i}: opcode={opcode}")

except Exception as e:
    print(f"\nPuzzle FAILED: {e}")
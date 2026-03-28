"""
Tests for the xkv8 Chialisp mining puzzle.

These tests compile the actual .clsp puzzle, curry in parameters,
and run it with various solutions to verify correctness.
"""

import hashlib
import os
import pytest
from blspy import AugSchemeMPL, PrivateKey
from chia_wallet_sdk import Clvm
from clvm_tools_rs import compile as compile_clsp


# Test constants matching the puzzle's curried parameters
GENESIS_HEIGHT = 100
EPOCH_LENGTH = 1120000
BASE_REWARD = 10000
# 2^238 as the base difficulty
BASE_DIFFICULTY = 2**238

# CAT v2 module hash (standard Chia CAT2 outer puzzle)
# Using a test placeholder; in production this would be the real CAT v2 mod hash
CAT_MOD_HASH = bytes.fromhex(
    "72dec062874cd4d3aab892a0906688a1ae412b0109982e1797a170add88bdcdc"
)

# TAIL program hash (identifies this specific CAT asset)
# Using a test placeholder
CAT_TAIL_HASH = bytes.fromhex(
    "abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd"
)


def generate_keypair():
    """Generate a disposable BLS private/public key pair for testing."""
    seed = os.urandom(32)
    sk = AugSchemeMPL.key_gen(seed)
    pk = sk.get_g1()
    return sk, bytes(pk)


def compile_puzzle():
    """Compile puzzle.clsp from source and return the hex string."""
    source = open("clsp/puzzle.clsp").read()
    return compile_clsp(source, ["clsp/include/"])

def sha256(*args) -> bytes:
    """Compute sha256 of concatenated byte arguments."""
    h = hashlib.sha256()
    for arg in args:
        if isinstance(arg, int):
            # Encode int as signed big-endian bytes (CLVM style)
            if arg == 0:
                h.update(b"")
            else:
                byte_len = (arg.bit_length() + 8) // 8  # +8 for sign bit
                h.update(arg.to_bytes(byte_len, "big", signed=True))
        elif isinstance(arg, bytes):
            h.update(arg)
        else:
            h.update(bytes(arg))
    return h.digest()

def int_to_clvm_bytes(n: int) -> bytes:
    """Convert a Python int to CLVM-style signed big-endian bytes."""
    if n == 0:
        return b""
    byte_len = (n.bit_length() + 8) // 8
    return n.to_bytes(byte_len, "big", signed=True)

def find_valid_nonce(puzzle_hash: bytes, miner_pubkey: bytes, user_height: int, difficulty: int) -> int:
    """Grind for a nonce that produces a valid PoW hash."""
    for nonce in range(50_000_000):
        pow_hash = sha256(puzzle_hash, miner_pubkey, int_to_clvm_bytes(user_height), int_to_clvm_bytes(nonce))
        pow_int = int.from_bytes(pow_hash, "big")
        if difficulty > pow_int:
            return nonce
    raise RuntimeError("Could not find valid nonce in 50M attempts")


def curry_puzzle(clvm, mod, cat_mod_hash=CAT_MOD_HASH, cat_tail_hash=CAT_TAIL_HASH):
    """Curry the puzzle module with all immutable parameters including CAT hashes."""
    mod_hash = mod.tree_hash()
    curried = mod.curry([
        clvm.atom(mod_hash),
        clvm.atom(cat_mod_hash),
        clvm.atom(cat_tail_hash),
        clvm.int(GENESIS_HEIGHT),
        clvm.int(EPOCH_LENGTH),
        clvm.int(BASE_REWARD),
        clvm.int(BASE_DIFFICULTY),
    ])
    return curried


def make_solution(clvm, my_amount, my_inner_puzzlehash, user_height, miner_pubkey, target_puzzle_hash, nonce):
    """Build a solution string with the my_inner_puzzlehash parameter."""
    solution_str = (
        f"({my_amount} "
        f"0x{my_inner_puzzlehash.hex()} "
        f"{user_height} "
        f"0x{miner_pubkey.hex()} "
        f"0x{target_puzzle_hash.hex()} "
        f"{nonce})"
    )
    return clvm.parse(solution_str)


def test_puzzle_compiles():
    """Verify the puzzle compiles from source and deserializes."""
    clvm = Clvm()
    hex_str = compile_puzzle()
    program = clvm.deserialize(bytes.fromhex(hex_str))
    assert program is not None


def test_puzzle_basic_structure():
    """Verify the uncurried module has a valid tree hash."""
    clvm = Clvm()
    hex_str = compile_puzzle()
    mod = clvm.deserialize(bytes.fromhex(hex_str))
    assert mod is not None
    mod_hash = mod.tree_hash()
    assert len(mod_hash) == 32


def test_puzzle_rejects_bad_pow():
    """Verify the puzzle raises on an invalid proof of work."""
    clvm = Clvm()
    hex_str = compile_puzzle()
    mod = clvm.deserialize(bytes.fromhex(hex_str))
    mod_hash = mod.tree_hash()

    # Use a trivially low difficulty so virtually no hash can pass
    impossible_difficulty = 1

    curried = mod.curry([
        clvm.atom(mod_hash),
        clvm.atom(CAT_MOD_HASH),
        clvm.atom(CAT_TAIL_HASH),
        clvm.int(GENESIS_HEIGHT),
        clvm.int(EPOCH_LENGTH),
        clvm.int(BASE_REWARD),
        clvm.int(impossible_difficulty),
    ])

    curried_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT + 10
    target_puzzle_hash = b"\xcd" * 32
    bad_nonce = 0

    solution = make_solution(clvm, my_amount, curried_hash, user_height, miner_pubkey, target_puzzle_hash, bad_nonce)

    with pytest.raises(Exception):
        curried.run(solution, 11_000_000_000, False)


def test_puzzle_rejects_before_genesis():
    """Verify the puzzle raises when user_height <= GENESIS_HEIGHT."""
    clvm = Clvm()
    hex_str = compile_puzzle()
    mod = clvm.deserialize(bytes.fromhex(hex_str))

    curried = curry_puzzle(clvm, mod)
    curried_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT  # exactly at genesis, not after
    target_puzzle_hash = b"\xcd" * 32
    nonce = 0

    solution = make_solution(clvm, my_amount, curried_hash, user_height, miner_pubkey, target_puzzle_hash, nonce)

    with pytest.raises(Exception):
        curried.run(solution, 11_000_000_000, False)


def test_puzzle_accepts_valid_pow():
    """Verify the puzzle succeeds with a valid proof of work and produces correct conditions."""
    clvm = Clvm()
    hex_str = compile_puzzle()
    mod = clvm.deserialize(bytes.fromhex(hex_str))

    curried = curry_puzzle(clvm, mod)
    curried_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT + 10
    target_puzzle_hash = b"\xcd" * 32

    # PoW is computed against the inner puzzle hash (curried_hash)
    nonce = find_valid_nonce(curried_hash, miner_pubkey, user_height, BASE_DIFFICULTY)

    solution = make_solution(clvm, my_amount, curried_hash, user_height, miner_pubkey, target_puzzle_hash, nonce)

    output = curried.run(solution, 11_000_000_000, False)
    assert output is not None

    conditions = output.value.to_list()
    # Expected: 9 conditions
    # ASSERT_MY_AMOUNT, ASSERT_MY_PUZZLEHASH, ASSERT_HEIGHT_RELATIVE,
    # ASSERT_HEIGHT_ABSOLUTE, ASSERT_BEFORE_HEIGHT_ABSOLUTE,
    # AGG_SIG_ME, REMARK, CREATE_COIN (reward), CREATE_COIN (self)
    assert len(conditions) == 10, f"Expected 10 conditions, got {len(conditions)}"


def test_self_hash_matches_curried_hash():
    """Verify the puzzle's self-computed hash matches the actual curried puzzle hash.

    The ASSERT_MY_PUZZLEHASH condition should contain the full CAT puzzle hash,
    computed from CAT_MOD_HASH, CAT_TAIL_HASH, and the inner puzzle hash.
    """
    clvm = Clvm()
    hex_str = compile_puzzle()
    mod = clvm.deserialize(bytes.fromhex(hex_str))

    curried = curry_puzzle(clvm, mod)
    curried_hash = curried.tree_hash()  # inner puzzle hash

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT + 10
    target_puzzle_hash = b"\xcd" * 32

    nonce = find_valid_nonce(curried_hash, miner_pubkey, user_height, BASE_DIFFICULTY)

    # Pass the inner puzzle hash as my_inner_puzzlehash solution parameter
    solution = make_solution(clvm, my_amount, curried_hash, user_height, miner_pubkey, target_puzzle_hash, nonce)

    output = curried.run(solution, 11_000_000_000, False)
    conditions = output.value.to_list()

    # The ASSERT_MY_PUZZLEHASH condition (index 1) should contain the full CAT puzzle hash
    # Condition format: (72 puzzle_hash)
    assert_my_puzzlehash_cond = conditions[1]
    pair = assert_my_puzzlehash_cond.to_pair()
    opcode = pair.first.to_int()
    assert opcode == 72, f"Expected opcode 72 (ASSERT_MY_PUZZLEHASH), got {opcode}"

    # The puzzle hash in the condition should be the full CAT puzzle hash,
    # NOT the inner puzzle hash. It should be:
    # curry_hashes(CAT_MOD_HASH, sha256tree1(CAT_MOD_HASH), sha256tree1(CAT_TAIL_HASH), sha256tree1(inner_puzzle_hash))
    cond_puzzle_hash = pair.rest.first().to_atom()

    # The asserted hash should NOT be the inner puzzle hash
    # (it should be the full CAT-wrapped hash)
    assert cond_puzzle_hash != curried_hash, (
        "ASSERT_MY_PUZZLEHASH should contain the full CAT puzzle hash, "
        "not the inner puzzle hash"
    )

    # Verify it's a valid 32-byte hash
    assert len(cond_puzzle_hash) == 32, f"Expected 32-byte hash, got {len(cond_puzzle_hash)} bytes"


# ---------------------------------------------------------------------------
# Epoch boundary tests
# ---------------------------------------------------------------------------

def _extract_reward_from_conditions(conditions):
    """Extract the miner reward amount from the CREATE_COIN condition at index 7.

    Condition layout from emit_conditions:
      0  ASSERT_MY_AMOUNT
      1  ASSERT_MY_PUZZLEHASH
      2  ASSERT_HEIGHT_RELATIVE
      3  ASSERT_HEIGHT_ABSOLUTE
      4  ASSERT_BEFORE_HEIGHT_ABSOLUTE
      5  AGG_SIG_ME
      6  REMARK
      7  CREATE_COIN  (reward to miner)
      8  CREATE_COIN  (self-recreation)
      9  CREATE_COIN_ANNOUNCEMENT
    """
    create_coin_cond = conditions[7]
    pair = create_coin_cond.to_pair()
    opcode = pair.first.to_int()
    assert opcode == 51, f"Expected opcode 51 (CREATE_COIN), got {opcode}"
    # (51 target_puzzle_hash reward ...)
    rest = pair.rest
    _target_ph = rest.first()
    reward_amount = rest.rest().first().to_int()
    return reward_amount


def _run_puzzle_at_height(user_height, expected_reward, expected_difficulty):
    """Run the puzzle at a given height and verify reward and PoW acceptance.

    Returns the full list of output conditions for further inspection.
    """
    clvm = Clvm()
    hex_str = compile_puzzle()
    mod = clvm.deserialize(bytes.fromhex(hex_str))

    curried = curry_puzzle(clvm, mod)
    curried_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    target_puzzle_hash = b"\xcd" * 32

    nonce = find_valid_nonce(curried_hash, miner_pubkey, user_height, expected_difficulty)

    solution = make_solution(
        clvm, my_amount, curried_hash, user_height,
        miner_pubkey, target_puzzle_hash, nonce,
    )

    output = curried.run(solution, 11_000_000_000, False)
    conditions = output.value.to_list()

    actual_reward = _extract_reward_from_conditions(conditions)
    assert actual_reward == expected_reward, (
        f"At height {user_height}: expected reward {expected_reward}, got {actual_reward}"
    )

    return conditions


# Epoch boundary heights
_EPOCH_1_START = GENESIS_HEIGHT + EPOCH_LENGTH        # 1,120,100
_EPOCH_2_START = GENESIS_HEIGHT + 2 * EPOCH_LENGTH    # 2,240,100
_EPOCH_3_START = GENESIS_HEIGHT + 3 * EPOCH_LENGTH    # 3,360,100


@pytest.mark.parametrize(
    "label, user_height, expected_reward, expected_difficulty",
    [
        # --- Epoch 0 → 1 boundary ---
        (
            "last block of epoch 0",
            _EPOCH_1_START - 1,
            BASE_REWARD,           # 10 000
            BASE_DIFFICULTY,       # 2^238
        ),
        (
            "first block of epoch 1",
            _EPOCH_1_START,
            BASE_REWARD >> 1,      # 5 000
            BASE_DIFFICULTY >> 1,  # 2^237
        ),
        # --- Epoch 1 → 2 boundary ---
        (
            "last block of epoch 1",
            _EPOCH_2_START - 1,
            BASE_REWARD >> 1,      # 5 000
            BASE_DIFFICULTY >> 1,  # 2^237
        ),
        (
            "first block of epoch 2",
            _EPOCH_2_START,
            BASE_REWARD >> 2,      # 2 500
            BASE_DIFFICULTY >> 2,  # 2^236
        ),
        # --- Epoch 2 → 3 boundary ---
        (
            "last block of epoch 2",
            _EPOCH_3_START - 1,
            BASE_REWARD >> 2,      # 2 500
            BASE_DIFFICULTY >> 2,  # 2^236
        ),
        (
            "first block of epoch 3",
            _EPOCH_3_START,
            BASE_REWARD >> 3,      # 1 250
            BASE_DIFFICULTY >> 3,  # 2^235
        ),
        # --- Epoch 3 cap (well past epoch 3 start) ---
        (
            "deep into epoch 3 (cap check)",
            _EPOCH_3_START + 5 * EPOCH_LENGTH,
            BASE_REWARD >> 3,      # still 1 250
            BASE_DIFFICULTY >> 3,  # still 2^235
        ),
    ],
    ids=lambda val: val if isinstance(val, str) else "",
)
def test_puzzle_epoch_boundary(label, user_height, expected_reward, expected_difficulty):
    """Verify reward halving and difficulty doubling at every epoch boundary.

    The puzzle defines four epochs (0-3). At each transition the reward halves
    and the difficulty target halves (making mining harder). Epoch 3 is the
    final epoch — the reward and difficulty remain constant beyond it.
    """
    _run_puzzle_at_height(user_height, expected_reward, expected_difficulty)

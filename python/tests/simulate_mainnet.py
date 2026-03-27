"""
Mainnet Simulation Script for xkv8

Simulates the full xkv8 puzzle execution using the current active mainnet
settings (GENESIS_HEIGHT, EPOCH_LENGTH, BASE_REWARD, BASE_DIFFICULTY) and
the real CAT module hash.  Validates that mining, spending, condition
generation, epoch transitions, and rejection of invalid inputs all behave
correctly under mainnet conditions.

Usage:
    python -m pytest python/tests/simulate_mainnet.py -v -s
"""

import hashlib
import os
import time

import pytest
from blspy import AugSchemeMPL
from chia_wallet_sdk import Clvm, Constants, cat_puzzle_hash
from clvm_tools_rs import compile as compile_clsp


# ── Mainnet constants (must match xkv8r.py) ─────────────────────────────

GENESIS_HEIGHT = 8521888
EPOCH_LENGTH = 1_120_000
BASE_REWARD = 10_000          # mojos
BASE_DIFFICULTY = 2 ** 238

CAT_TAIL_HASH = bytes.fromhex(
    "f09c8d630a0a64eb4633c0933e0ca131e646cebb384cfc4f6718bad80859b5e8"
)

PUZZLE_HEX = (
    "ff02ffff01ff02ff7effff04ff02ffff04ff8202ffffff04ffff02ff52ffff04ff02ffff04"
    "ff0bffff04ff17ffff04ff8205ffff808080808080ffff04ff8205ffffff04ff820bffffff"
    "04ff2fffff04ff8217ffffff04ff822fffffff04ff825fffffff04ffff02ff56ffff04ff02"
    "ffff04ff81bfffff04ffff02ff26ffff04ff02ffff04ff820bffffff04ff2fffff04ff5fff"
    "808080808080ff8080808080ffff04ffff02ff7affff04ff02ffff04ff82017fffff04ffff"
    "02ff26ffff04ff02ffff04ff820bffffff04ff2fffff04ff5fff808080808080ff80808080"
    "80ff80808080808080808080808080ffff04ffff01ffffffff3257ff53ff5249ffff48ff33"
    "3cff01ff0102ffffffff02ffff03ff05ffff01ff0bff8201f2ffff02ff76ffff04ff02ffff"
    "04ff09ffff04ffff02ff22ffff04ff02ffff04ff0dff80808080ff808080808080ffff0182"
    "01b280ff0180ffff02ff2affff04ff02ffff04ff05ffff04ffff02ff5effff04ff02ffff04"
    "ff05ff80808080ffff04ffff02ff5effff04ff02ffff04ff0bff80808080ffff04ff17ff80"
    "808080808080ffffa04bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce2"
    "3c7785459aa09dcf97a184f32623d11a73124ceb99a5709b083721e878a16d78f596718b"
    "a7b2ffa102a12871fee210fb8619291eaea194581cbd2531e4b23759d225f6806923f6322"
    "2a102a8d5dd63fba471ebcb1f3e8f7c1e1879b7152a6e7298a91ce119a63400ade7c5fff"
    "f0bff820172ffff02ff76ffff04ff02ffff04ff05ffff04ffff02ff22ffff04ff02ffff04"
    "ff07ff80808080ff808080808080ffff04ffff04ff78ffff04ff05ff808080ffff04ffff04"
    "ff24ffff04ff0bff808080ffff04ffff04ff58ffff01ff018080ffff04ffff04ff28ffff04"
    "ff2fff808080ffff04ffff04ff30ffff04ffff10ff2fffff010380ff808080ffff04ffff04"
    "ff20ffff04ff5fffff04ffff0bff81bfff82017fff2f80ff80808080ffff04ffff04ff5cff"
    "ff04ff5fff808080ffff04ffff04ff54ffff04ff81bfffff04ff8202ffffff04ffff04ff81"
    "bfff8080ff8080808080ffff04ffff04ff54ffff04ff17ffff04ffff11ff05ff8202ff80ff"
    "ff04ffff04ff17ff8080ff8080808080ffff04ffff04ff74ffff01ff248080ff8080808080"
    "808080808080ff16ff05ffff11ff80ff0b8080ffffff02ffff03ffff15ffff05ffff14ffff"
    "11ff05ff0b80ff178080ffff010380ffff01ff0103ffff01ff05ffff14ffff11ff05ff0b80"
    "ff17808080ff0180ffff16ff05ffff11ff80ff0b8080ff0bff7cffff0bff7cff8201b2ff05"
    "80ffff0bff7cff0bff8201328080ffff02ffff03ffff15ff05ff8080ffff01ff15ff0bff05"
    "80ff8080ff0180ffff02ffff03ffff07ff0580ffff01ff0bff7cffff02ff5effff04ff02ff"
    "ff04ff09ff80808080ffff02ff5effff04ff02ffff04ff0dff8080808080ffff01ff0bff2c"
    "ff058080ff0180ff02ffff03ffff15ff2fff5f80ffff01ff02ffff03ffff02ff2effff04ff"
    "02ffff04ffff0bff17ff81bfff2fff8202ff80ffff04ff820bffff8080808080ffff01ff02"
    "ffff03ffff20ffff15ff8205ffff058080ffff01ff02ff5affff04ff02ffff04ff05ffff04"
    "ff0bffff04ff17ffff04ff2fffff04ff81bfffff04ff82017fffff04ff8202ffffff04ff82"
    "05ffff8080808080808080808080ffff01ff088080ff0180ffff01ff088080ff0180ffff01"
    "ff088080ff0180ff018080"
)


# ── Helpers ──────────────────────────────────────────────────────────────

def int_to_clvm_bytes(n: int) -> bytes:
    if n == 0:
        return b""
    byte_len = (n.bit_length() + 8) // 8
    return n.to_bytes(byte_len, "big", signed=True)


def sha256_concat(*args) -> bytes:
    h = hashlib.sha256()
    for arg in args:
        if isinstance(arg, int):
            h.update(int_to_clvm_bytes(arg))
        elif isinstance(arg, bytes):
            h.update(arg)
        else:
            h.update(bytes(arg))
    return h.digest()


def get_epoch(user_height: int) -> int:
    raw = (user_height - GENESIS_HEIGHT) // EPOCH_LENGTH
    return min(raw, 3)


def get_reward(epoch: int) -> int:
    return BASE_REWARD >> epoch


def get_difficulty(epoch: int) -> int:
    return BASE_DIFFICULTY >> epoch


def find_valid_nonce(puzzle_hash, miner_pubkey, user_height, difficulty, max_attempts=10_000_000):
    h_bytes = int_to_clvm_bytes(user_height)
    for nonce in range(max_attempts):
        n_bytes = int_to_clvm_bytes(nonce)
        digest = hashlib.sha256(puzzle_hash + miner_pubkey + h_bytes + n_bytes).digest()
        pow_int = int.from_bytes(digest, "big")
        if pow_int > 0 and difficulty > pow_int:
            return nonce
    raise RuntimeError(f"Could not find valid nonce in {max_attempts:,} attempts")


def generate_keypair():
    seed = os.urandom(32)
    sk = AugSchemeMPL.key_gen(seed)
    pk = sk.get_g1()
    return sk, bytes(pk)


def compile_puzzle():
    source = open("clsp/puzzle.clsp").read()
    return compile_clsp(source, ["clsp/include/"])


def curry_puzzle_mainnet(clvm, mod):
    mod_hash = mod.tree_hash()
    cat_mod_hash = Constants.cat_puzzle_hash()
    curried = mod.curry([
        clvm.atom(mod_hash),
        clvm.atom(cat_mod_hash),
        clvm.atom(CAT_TAIL_HASH),
        clvm.int(GENESIS_HEIGHT),
        clvm.int(EPOCH_LENGTH),
        clvm.int(BASE_REWARD),
        clvm.int(BASE_DIFFICULTY),
    ])
    return curried


def make_solution(clvm, my_amount, my_inner_puzzlehash, user_height, miner_pubkey, target_puzzle_hash, nonce):
    solution_str = (
        f"({my_amount} "
        f"0x{my_inner_puzzlehash.hex()} "
        f"{user_height} "
        f"0x{miner_pubkey.hex()} "
        f"0x{target_puzzle_hash.hex()} "
        f"{nonce})"
    )
    return clvm.parse(solution_str)


# ── Condition opcodes ────────────────────────────────────────────────────

ASSERT_MY_AMOUNT = 73
ASSERT_MY_PUZZLEHASH = 72
ASSERT_HEIGHT_RELATIVE = 82
ASSERT_HEIGHT_ABSOLUTE = 83
ASSERT_BEFORE_HEIGHT_ABSOLUTE = 87
AGG_SIG_ME = 50
OP_REMARK = 1
CREATE_COIN = 51
CREATE_COIN_ANNOUNCEMENT = 60


# ══════════════════════════════════════════════════════════════════════════
# Tests
# ══════════════════════════════════════════════════════════════════════════

def test_compiled_hex_matches_source():
    """Verify the compiled puzzle hex in xkv8r.py matches fresh compilation."""
    compiled_hex = compile_puzzle()
    assert compiled_hex == PUZZLE_HEX, (
        "Compiled puzzle hex does not match PUZZLE_HEX from xkv8r.py. "
        "The on-chain puzzle may differ from the source!"
    )
    print("[PASS] Compiled hex matches PUZZLE_HEX")


def test_mainnet_constants_consistency():
    """Verify mainnet constants are self-consistent and reasonable."""
    assert GENESIS_HEIGHT > 0
    assert EPOCH_LENGTH > 0
    assert BASE_REWARD > 0
    assert BASE_DIFFICULTY > 0
    assert len(CAT_TAIL_HASH) == 32

    # Epoch 0 values
    assert get_epoch(GENESIS_HEIGHT + 1) == 0
    assert get_reward(0) == BASE_REWARD
    assert get_difficulty(0) == BASE_DIFFICULTY

    # Epoch boundaries
    assert get_epoch(GENESIS_HEIGHT + EPOCH_LENGTH - 1) == 0
    assert get_epoch(GENESIS_HEIGHT + EPOCH_LENGTH) == 1
    assert get_epoch(GENESIS_HEIGHT + 2 * EPOCH_LENGTH) == 2
    assert get_epoch(GENESIS_HEIGHT + 3 * EPOCH_LENGTH) == 3
    assert get_epoch(GENESIS_HEIGHT + 100 * EPOCH_LENGTH) == 3  # capped

    # Halving
    assert get_reward(1) == BASE_REWARD // 2
    assert get_reward(2) == BASE_REWARD // 4
    assert get_reward(3) == BASE_REWARD // 8

    assert get_difficulty(1) == BASE_DIFFICULTY // 2
    assert get_difficulty(2) == BASE_DIFFICULTY // 4
    assert get_difficulty(3) == BASE_DIFFICULTY // 8

    print("[PASS] Mainnet constants are consistent")
    print(f"  GENESIS_HEIGHT = {GENESIS_HEIGHT:,}")
    print(f"  EPOCH_LENGTH   = {EPOCH_LENGTH:,}")
    print(f"  BASE_REWARD    = {BASE_REWARD:,}")
    print(f"  BASE_DIFFICULTY= 2^{BASE_DIFFICULTY.bit_length()-1}")
    print(f"  CAT_TAIL_HASH  = {CAT_TAIL_HASH.hex()}")


def test_mainnet_puzzle_curries_correctly():
    """Verify the puzzle curries with mainnet params and produces a valid hash."""
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()

    assert len(inner_hash) == 32
    assert inner_hash != mod.tree_hash(), "Curried hash should differ from uncurried"

    # Verify full CAT puzzle hash derivation
    full_cat_ph = cat_puzzle_hash(CAT_TAIL_HASH, inner_hash)
    assert len(full_cat_ph) == 32
    assert full_cat_ph != inner_hash

    print("[PASS] Puzzle curries correctly with mainnet params")
    print(f"  Inner puzzle hash    : {inner_hash.hex()}")
    print(f"  Full CAT puzzle hash : {full_cat_ph.hex()}")


def test_mainnet_epoch0_valid_pow_and_conditions():
    """
    Full mainnet simulation at epoch 0 (just after genesis):
    Mine a valid nonce, run the puzzle, and verify all output conditions.
    """
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()
    cat_mod_hash = Constants.cat_puzzle_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000  # total lode supply in mojos
    user_height = GENESIS_HEIGHT + 1  # first mineable height
    target_puzzle_hash = b"\xcd" * 32
    epoch = get_epoch(user_height)
    reward = get_reward(epoch)
    difficulty = get_difficulty(epoch)

    assert epoch == 0
    assert reward == BASE_REWARD
    assert difficulty == BASE_DIFFICULTY

    print(f"\n{'='*60}")
    print(f"  Mainnet Epoch 0 Simulation")
    print(f"{'='*60}")
    print(f"  user_height : {user_height:,}")
    print(f"  epoch       : {epoch}")
    print(f"  reward      : {reward:,} mojos")
    print(f"  difficulty  : 2^{difficulty.bit_length()-1}")

    t0 = time.perf_counter()
    nonce = find_valid_nonce(inner_hash, miner_pubkey, user_height, difficulty)
    elapsed = time.perf_counter() - t0
    print(f"  nonce found : {nonce} in {elapsed:.3f}s")

    # Verify PoW independently
    pow_hash = sha256_concat(inner_hash, miner_pubkey, user_height, nonce)
    pow_int = int.from_bytes(pow_hash, "big")
    assert pow_int > 0, "PoW hash must be positive"
    assert difficulty > pow_int, "PoW hash must be less than difficulty"
    print(f"  PoW hash    : {pow_hash.hex()[:32]}...")
    print(f"  PoW valid   : YES")

    # Run the puzzle
    solution = make_solution(clvm, my_amount, inner_hash, user_height, miner_pubkey, target_puzzle_hash, nonce)
    output = curried.run(solution, 11_000_000_000, False)
    conditions = output.value.to_list()

    print(f"  conditions  : {len(conditions)}")

    # Parse and verify each condition
    cond_map = {}
    for cond in conditions:
        pair = cond.to_pair()
        opcode = pair.first.to_int()
        cond_map.setdefault(opcode, []).append(cond)

    # ASSERT_MY_AMOUNT
    assert ASSERT_MY_AMOUNT in cond_map
    amt_cond = cond_map[ASSERT_MY_AMOUNT][0].to_pair()
    assert amt_cond.rest.first().to_int() == my_amount
    print(f"  [OK] ASSERT_MY_AMOUNT = {my_amount:,}")

    # ASSERT_MY_PUZZLEHASH (should be full CAT puzzle hash)
    assert ASSERT_MY_PUZZLEHASH in cond_map
    ph_cond = cond_map[ASSERT_MY_PUZZLEHASH][0].to_pair()
    asserted_ph = ph_cond.rest.first().to_atom()
    assert len(asserted_ph) == 32
    assert asserted_ph != inner_hash, "Should be full CAT hash, not inner hash"
    print(f"  [OK] ASSERT_MY_PUZZLEHASH = full CAT hash")

    # ASSERT_HEIGHT_RELATIVE
    assert ASSERT_HEIGHT_RELATIVE in cond_map
    hr_cond = cond_map[ASSERT_HEIGHT_RELATIVE][0].to_pair()
    assert hr_cond.rest.first().to_int() == 1
    print(f"  [OK] ASSERT_HEIGHT_RELATIVE = 1")

    # ASSERT_HEIGHT_ABSOLUTE
    assert ASSERT_HEIGHT_ABSOLUTE in cond_map
    ha_cond = cond_map[ASSERT_HEIGHT_ABSOLUTE][0].to_pair()
    assert ha_cond.rest.first().to_int() == user_height
    print(f"  [OK] ASSERT_HEIGHT_ABSOLUTE = {user_height:,}")

    # ASSERT_BEFORE_HEIGHT_ABSOLUTE
    assert ASSERT_BEFORE_HEIGHT_ABSOLUTE in cond_map
    bha_cond = cond_map[ASSERT_BEFORE_HEIGHT_ABSOLUTE][0].to_pair()
    assert bha_cond.rest.first().to_int() == user_height + 3
    print(f"  [OK] ASSERT_BEFORE_HEIGHT_ABSOLUTE = {user_height + 3:,}")

    # AGG_SIG_ME
    assert AGG_SIG_ME in cond_map
    sig_cond = cond_map[AGG_SIG_ME][0].to_pair()
    sig_pk = sig_cond.rest.first().to_atom()
    assert sig_pk == miner_pubkey
    sig_msg = sig_cond.rest.rest().first().to_atom()
    expected_msg = sha256_concat(target_puzzle_hash, nonce, user_height)
    assert sig_msg == expected_msg
    print(f"  [OK] AGG_SIG_ME pubkey + message correct")

    # REMARK
    assert OP_REMARK in cond_map
    remark_cond = cond_map[OP_REMARK][0].to_pair()
    remark_pk = remark_cond.rest.first().to_atom()
    assert remark_pk == miner_pubkey
    print(f"  [OK] REMARK contains miner pubkey")

    # CREATE_COIN (should have 2: reward + self-recreation)
    assert CREATE_COIN in cond_map
    create_coins = cond_map[CREATE_COIN]
    assert len(create_coins) == 2, f"Expected 2 CREATE_COIN, got {len(create_coins)}"

    # Reward coin
    cc0 = create_coins[0].to_pair()
    cc0_ph = cc0.rest.first().to_atom()
    cc0_amt = cc0.rest.rest().first().to_int()
    assert cc0_ph == target_puzzle_hash
    assert cc0_amt == reward
    print(f"  [OK] CREATE_COIN reward = {reward:,} to target")

    # Self-recreation coin
    cc1 = create_coins[1].to_pair()
    cc1_ph = cc1.rest.first().to_atom()
    cc1_amt = cc1.rest.rest().first().to_int()
    assert cc1_ph == inner_hash
    assert cc1_amt == my_amount - reward
    print(f"  [OK] CREATE_COIN self = {my_amount - reward:,} (remaining)")

    # CREATE_COIN_ANNOUNCEMENT
    assert CREATE_COIN_ANNOUNCEMENT in cond_map
    print(f"  [OK] CREATE_COIN_ANNOUNCEMENT present")

    print(f"\n{'='*60}")
    print(f"  Epoch 0 Simulation PASSED")
    print(f"{'='*60}\n")


def test_mainnet_rejects_at_genesis_height():
    """Puzzle must reject user_height == GENESIS_HEIGHT (needs to be strictly greater)."""
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT  # exactly at genesis, not after
    target_puzzle_hash = b"\xcd" * 32

    solution = make_solution(clvm, my_amount, inner_hash, user_height, miner_pubkey, target_puzzle_hash, 0)

    with pytest.raises(Exception):
        curried.run(solution, 11_000_000_000, False)

    print("[PASS] Puzzle rejects user_height == GENESIS_HEIGHT")


def test_mainnet_rejects_before_genesis():
    """Puzzle must reject user_height < GENESIS_HEIGHT."""
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT - 100
    target_puzzle_hash = b"\xcd" * 32

    solution = make_solution(clvm, my_amount, inner_hash, user_height, miner_pubkey, target_puzzle_hash, 0)

    with pytest.raises(Exception):
        curried.run(solution, 11_000_000_000, False)

    print("[PASS] Puzzle rejects user_height < GENESIS_HEIGHT")


def test_mainnet_rejects_bad_pow():
    """Puzzle must reject a nonce that doesn't satisfy the difficulty target."""
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT + 1
    target_puzzle_hash = b"\xcd" * 32
    bad_nonce = 999999999  # almost certainly won't satisfy 2^238

    # Verify this nonce is actually bad
    pow_hash = sha256_concat(inner_hash, miner_pubkey, user_height, bad_nonce)
    pow_int = int.from_bytes(pow_hash, "big")
    if pow_int > 0 and BASE_DIFFICULTY > pow_int:
        pytest.skip("Nonce accidentally valid")

    solution = make_solution(clvm, my_amount, inner_hash, user_height, miner_pubkey, target_puzzle_hash, bad_nonce)

    with pytest.raises(Exception):
        curried.run(solution, 11_000_000_000, False)

    print("[PASS] Puzzle rejects invalid proof of work")


def test_mainnet_rejects_insufficient_balance():
    """Puzzle must reject when coin amount < reward."""
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = BASE_REWARD - 1  # less than reward
    user_height = GENESIS_HEIGHT + 1
    target_puzzle_hash = b"\xcd" * 32
    difficulty = get_difficulty(0)

    nonce = find_valid_nonce(inner_hash, miner_pubkey, user_height, difficulty)
    solution = make_solution(clvm, my_amount, inner_hash, user_height, miner_pubkey, target_puzzle_hash, nonce)

    with pytest.raises(Exception):
        curried.run(solution, 11_000_000_000, False)

    print("[PASS] Puzzle rejects insufficient balance")


def test_mainnet_epoch_transitions():
    """
    Verify the puzzle produces correct rewards and difficulties across
    all four epochs using mainnet parameters.
    """
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    target_puzzle_hash = b"\xcd" * 32

    print(f"\n{'='*60}")
    print(f"  Epoch Transition Simulation")
    print(f"{'='*60}")

    for epoch in range(4):
        user_height = GENESIS_HEIGHT + 1 + (epoch * EPOCH_LENGTH)
        expected_reward = get_reward(epoch)
        difficulty = get_difficulty(epoch)

        assert get_epoch(user_height) == epoch

        t0 = time.perf_counter()
        nonce = find_valid_nonce(inner_hash, miner_pubkey, user_height, difficulty)
        elapsed = time.perf_counter() - t0

        solution = make_solution(clvm, my_amount, inner_hash, user_height, miner_pubkey, target_puzzle_hash, nonce)
        output = curried.run(solution, 11_000_000_000, False)
        conditions = output.value.to_list()

        # Find CREATE_COIN conditions
        reward_amount = None
        remainder_amount = None
        for cond in conditions:
            pair = cond.to_pair()
            opcode = pair.first.to_int()
            if opcode == CREATE_COIN:
                ph = pair.rest.first().to_atom()
                amt = pair.rest.rest().first().to_int()
                if ph == target_puzzle_hash:
                    reward_amount = amt
                elif ph == inner_hash:
                    remainder_amount = amt

        assert reward_amount == expected_reward, (
            f"Epoch {epoch}: expected reward {expected_reward}, got {reward_amount}"
        )
        assert remainder_amount == my_amount - expected_reward, (
            f"Epoch {epoch}: expected remainder {my_amount - expected_reward}, got {remainder_amount}"
        )

        print(
            f"  Epoch {epoch}: height={user_height:,}  "
            f"reward={expected_reward:,}  "
            f"difficulty=2^{difficulty.bit_length()-1}  "
            f"nonce={nonce} ({elapsed:.3f}s)"
        )

    print(f"\n{'='*60}")
    print(f"  Epoch Transition Simulation PASSED")
    print(f"{'='*60}\n")


def test_mainnet_agg_sig_message_correctness():
    """
    Verify the AGG_SIG_ME message matches what the miner would sign,
    ensuring signature verification will succeed on mainnet.
    """
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()

    _sk, miner_pubkey = generate_keypair()
    my_amount = 21_000_000_000
    user_height = GENESIS_HEIGHT + 42
    target_puzzle_hash = os.urandom(32)
    difficulty = get_difficulty(0)

    nonce = find_valid_nonce(inner_hash, miner_pubkey, user_height, difficulty)
    solution = make_solution(clvm, my_amount, inner_hash, user_height, miner_pubkey, target_puzzle_hash, nonce)
    output = curried.run(solution, 11_000_000_000, False)
    conditions = output.value.to_list()

    # Extract AGG_SIG_ME condition
    for cond in conditions:
        pair = cond.to_pair()
        if pair.first.to_int() == AGG_SIG_ME:
            sig_pk = pair.rest.first().to_atom()
            sig_msg = pair.rest.rest().first().to_atom()

            # The message should be sha256(target_puzzle_hash + nonce + user_height)
            expected = sha256_concat(target_puzzle_hash, nonce, user_height)
            assert sig_msg == expected, (
                f"AGG_SIG_ME message mismatch:\n"
                f"  got:      {sig_msg.hex()}\n"
                f"  expected: {expected.hex()}"
            )
            assert sig_pk == miner_pubkey
            print("[PASS] AGG_SIG_ME message is correct")
            print(f"  message = sha256(target_ph || nonce || height)")
            print(f"  = {expected.hex()}")
            return

    pytest.fail("AGG_SIG_ME condition not found")


def test_mainnet_full_lifecycle_summary():
    """
    End-to-end summary: curry, mine, run, verify conditions, then
    simulate the self-recreation by mining the next block.
    """
    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    curried = curry_puzzle_mainnet(clvm, mod)
    inner_hash = curried.tree_hash()
    full_cat_ph = cat_puzzle_hash(CAT_TAIL_HASH, inner_hash)

    _sk, miner_pubkey = generate_keypair()
    initial_amount = 21_000_000_000
    target_puzzle_hash = b"\xab" * 32

    print(f"\n{'='*60}")
    print(f"  Full Mainnet Lifecycle Simulation")
    print(f"{'='*60}")
    print(f"  Inner puzzle hash : {inner_hash.hex()}")
    print(f"  Full CAT ph       : {full_cat_ph.hex()}")
    print(f"  Initial supply    : {initial_amount:,} mojos")
    print()

    remaining = initial_amount

    # Simulate mining 2 consecutive blocks
    for block_num in range(1, 3):
        user_height = GENESIS_HEIGHT + block_num
        epoch = get_epoch(user_height)
        reward = get_reward(epoch)
        difficulty = get_difficulty(epoch)

        print(f"  --- Block {block_num} (height {user_height:,}) ---")

        t0 = time.perf_counter()
        nonce = find_valid_nonce(inner_hash, miner_pubkey, user_height, difficulty)
        elapsed = time.perf_counter() - t0
        print(f"    nonce: {nonce} ({elapsed:.3f}s)")

        solution = make_solution(
            clvm, remaining, inner_hash, user_height,
            miner_pubkey, target_puzzle_hash, nonce,
        )
        output = curried.run(solution, 11_000_000_000, False)
        conditions = output.value.to_list()

        # Verify CREATE_COIN conditions
        found_reward = False
        found_self = False
        for cond in conditions:
            pair = cond.to_pair()
            opcode = pair.first.to_int()
            if opcode == CREATE_COIN:
                ph = pair.rest.first().to_atom()
                amt = pair.rest.rest().first().to_int()
                if ph == target_puzzle_hash:
                    assert amt == reward
                    found_reward = True
                    print(f"    reward: {amt:,} mojos -> target")
                elif ph == inner_hash:
                    assert amt == remaining - reward
                    found_self = True
                    print(f"    self:   {amt:,} mojos (remaining)")

        assert found_reward, "Missing reward CREATE_COIN"
        assert found_self, "Missing self-recreation CREATE_COIN"

        remaining -= reward
        print(f"    supply after: {remaining:,}")
        print()

    # Verify total deducted matches expected
    total_mined = initial_amount - remaining
    assert total_mined == BASE_REWARD * 2  # 2 blocks at epoch 0
    print(f"  Total mined: {total_mined:,} mojos across 2 blocks")

    print(f"\n{'='*60}")
    print(f"  Full Lifecycle Simulation PASSED")
    print(f"{'='*60}\n")

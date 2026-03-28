from __future__ import annotations

import asyncio
import hashlib
import os
import signal
import sys
from typing import Optional

from chia_wallet_sdk import (
    Address,
    CatSpend,
    Clvm,
    RpcClient,
    Constants,
    SecretKey,
    Spend,
    SpendBundle,
    cat_puzzle_hash,
)

# ── Environment ──────────────────────────────────────────────────────────
#
# TARGET_ADDRESS: the address to receive mining rewards.  Must be set in the environment before running.
# MINER_SECRET_KEY: optional 32-byte hex seed for miner's BLS secret key.  If not set, your leaderboard standings will be incorrect.
#
# a MUST-set value
TARGET_ADDRESS = os.environ.get("TARGET_ADDRESS", None)
if TARGET_ADDRESS is None:
    print("Error: Required TARGET_ADDRESS environment variable not set")
    sys.exit(1)
TARGET_PUZZLEHASH = Address.decode(TARGET_ADDRESS).puzzle_hash

# Miner secret key: 32-byte hex seed. Any valid BLS key will do, but hold on to it to manage your leaderboard nickname!
_MINER_KEY_HEX = os.environ.get("MINER_SECRET_KEY", "")

TESTNET = os.environ.get("TESTNET", None)

if TESTNET is None and not TARGET_ADDRESS.startswith("xch"):
    print("Error: TARGET_ADDRESS must be a mainnet address (starting with 'xch')")
    sys.exit(1)
elif TESTNET is not None and not TARGET_ADDRESS.startswith("txch"):
    print("Error: TARGET_ADDRESS must be a testnet address (starting with 'txch')")
    sys.exit(1)

# ──────────────────────────────────────────────────────────────────────────

# ── Puzzle parameters (differ by network) ────────────────────────────
CAT_TAIL_HASH = bytes.fromhex(
    "f09c8d630a0a64eb4633c0933e0ca131e646cebb384cfc4f6718bad80859b5e8"
)

# you could change these...but then your miner won't find anything to mine.
# left here as an exercise for anyone who wants to build their own mineable CAT
GENESIS_HEIGHT = 8521888
EPOCH_LENGTH = 1_120_000
BASE_REWARD = 10_000  # mojos
BASE_DIFFICULTY = 2**238

# coin_id -> mine_height of last successful submission
submitted_coins: dict[bytes, int] = {}

CLVM = Clvm()


# ── Compiled puzzle (puzzle.clsp) ────────────────────────────────────────
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


def build_curried_puzzle(clvm: Clvm):
    """Curry the compiled puzzle and return (curried_program, inner_puzzle_hash)."""
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
    mod_hash = mod.tree_hash()

    cat_mod_hash = Constants.cat_puzzle_hash()  # standard CAT v2 mod hash

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
    return curried, inner_puzzle_hash, cat_mod_hash


# ── Miner key helpers ───────────────────────────────────────────────────

def load_miner_key() -> SecretKey:
    """Load or generate the miner's BLS secret key."""
    if _MINER_KEY_HEX:
        seed = bytes.fromhex(_MINER_KEY_HEX)
        return SecretKey.from_seed(seed)
    # deterministic fallback so the key is stable within a single run
    seed = os.urandom(32)
    print(f"No MINER_SECRET_KEY set – generated ephemeral key. You will be able to mine, but leaderboard standings will be impacted!")
    return SecretKey.from_seed(seed)


# ── PoW helpers ─────────────────────────────────────────────────────────

def int_to_clvm_bytes(n: int) -> bytes:
    """Encode a Python int as CLVM-style signed big-endian bytes."""
    if n == 0:
        return b""
    byte_len = (n.bit_length() + 8) // 8
    return n.to_bytes(byte_len, "big", signed=True)


def pow_sha256(*args) -> bytes:
    """SHA-256 of concatenated args (bytes or ints encoded CLVM-style)."""
    h = hashlib.sha256()
    for arg in args:
        if isinstance(arg, int):
            h.update(int_to_clvm_bytes(arg))
        else:
            h.update(arg)
    return h.digest()


def get_epoch(user_height: int) -> int:
    raw = (user_height - GENESIS_HEIGHT) // EPOCH_LENGTH
    return min(raw, 3)


def get_reward(epoch: int) -> int:
    return BASE_REWARD >> epoch


def get_difficulty(epoch: int) -> int:
    return BASE_DIFFICULTY >> epoch


def find_valid_nonce(
    inner_puzzle_hash: bytes,
    miner_pubkey_bytes: bytes,
    user_height: int,
    difficulty: int,
    max_attempts: int = 5_000_000,
) -> Optional[int]:
    """Grind for a nonce that satisfies the PoW target."""
    h_bytes = int_to_clvm_bytes(user_height)
    for nonce in range(max_attempts):
        n_bytes = int_to_clvm_bytes(nonce)
        digest = hashlib.sha256(
            inner_puzzle_hash + miner_pubkey_bytes + h_bytes + n_bytes
        ).digest()
        pow_int = int.from_bytes(digest, "big")
        # must be positive (first byte < 0x80) AND less than difficulty
        if pow_int > 0 and difficulty > pow_int:
            return nonce
    return None


# ── Main mining loop ────────────────────────────────────────────────────

# Known genesis challenges by network name
GENESIS_CHALLENGES = {
    "testnet11": bytes.fromhex(
        "37a90eb5185a9c4439a91ddc98bbadce7b4feba060d50116a067de66bf236615"
    ),
    "mainnet": bytes.fromhex(
        "ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb"
    ),
}


async def check_mining_results(client: RpcClient, inner_puzzle_hash: bytes):
    """Check if any previously submitted coins were mined, and whether by us."""
    to_remove = []
    for coin_id, sub_height in list(submitted_coins.items()):
        try:
            cr_res = await client.get_coin_record_by_name(coin_id)
            if not cr_res.success or cr_res.coin_record is None:
                to_remove.append(coin_id)
                continue
            if not cr_res.coin_record.spent:
                continue
            # Coin was spent — determine if the reward went to our address
            to_remove.append(coin_id)
            spent_cr = cr_res.coin_record
            gps_res = await client.get_puzzle_and_solution(
                coin_id, spent_cr.spent_block_index
            )
            if not gps_res.success:
                print(f"Coin {coin_id.hex()[:16]}… spent at height {spent_cr.spent_block_index} but could not retrieve solution")
                continue
            puzzle = CLVM.deserialize(gps_res.coin_solution.puzzle_reveal).puzzle()
            solution = CLVM.deserialize(gps_res.coin_solution.solution)
            cats = puzzle.parse_child_cats(spent_cr.coin, solution)
            if cats is None:
                print(f"Coin {coin_id.hex()[:16]}… spent but could not parse child CATs")
                continue
            reward_mojos = 0
            for cat in cats:
                if cat.info.p2_puzzle_hash == TARGET_PUZZLEHASH:
                    reward_mojos = cat.coin.amount
                    break
            mined_by_us = reward_mojos > 0
            if mined_by_us:
                excavator = r"""
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
"""
                print(excavator)
                print(f"Win CONFIRMED at height {sub_height}!")
                reward_cat_amount = reward_mojos / 1000
                print(f"Reward of {reward_cat_amount:.3f} XKV8 sent to {TARGET_ADDRESS}")
                print()
            else:
                print(f"Coin submitted at height {sub_height} was mined by another miner")
        except Exception as e:
            to_remove.append(coin_id)
            print(f"Could not verify mining result for {coin_id.hex()[:16]}…: {repr(e)}")
    for coin_id in to_remove:
        submitted_coins.pop(coin_id, None)


async def mine():
    client = None
    if TESTNET is not None:
        client = RpcClient.testnet11()
    else:
        client = RpcClient.mainnet()

    # Get genesis challenge for AGG_SIG_ME
    net_info = await client.get_network_info()
    genesis_challenge = net_info.genesis_challenge
    if genesis_challenge is None and net_info.network_name is not None:
        genesis_challenge = GENESIS_CHALLENGES.get(net_info.network_name)
    if genesis_challenge is None:
        print(
            f"Failed to get genesis challenge "
            f"(success={net_info.success}, network_name={net_info.network_name}, "
            f"error={net_info.error})"
        )
        return
    #print(f"Genesis challenge: {genesis_challenge.hex()}")

    # Compile & curry the puzzle once
    curried_puzzle, inner_puzzle_hash, cat_mod_hash = build_curried_puzzle(CLVM)
    full_cat_puzzlehash = cat_puzzle_hash(CAT_TAIL_HASH, inner_puzzle_hash)

    print(f"Lode puzzle hash: {inner_puzzle_hash.hex()}")
    print(f"Lode full CAT puzzle hash: {full_cat_puzzlehash.hex()}")
    #print(f"CAT TAIL hash:     {CAT_TAIL_HASH.hex()}")

    # Load miner key
    sk = load_miner_key()
    pk = sk.public_key()
    pk_bytes = pk.to_bytes()
    print(f"Miner public key: {pk_bytes.hex()}")
    print(f"Mining to address: {TARGET_ADDRESS}")
    print()

    last_height = -1

    while True:
        blockchain_state = await client.get_blockchain_state()
        if not blockchain_state.success:
            print("Failed to get blockchain state")
            await asyncio.sleep(5)
            continue

        height = blockchain_state.blockchain_state.peak.height
        if height != last_height:
            last_height = height
            if height % 100 == 0:
                print(f"Height: {height}")

            # Check if any previously submitted coins were confirmed
            if submitted_coins:
                await check_mining_results(client, inner_puzzle_hash)

        # Search for unspent lode coins by puzzle hash
        unspent_crs = await client.get_coin_records_by_puzzle_hash(
            full_cat_puzzlehash, None, None, False,
        )
        if not unspent_crs.success:
            print("Failed to discover unspent coins")
            await asyncio.sleep(5)
            continue

        # Don't attempt mining before genesis height
        mine_height = 1 + last_height
        if mine_height < GENESIS_HEIGHT:
            print(f"Waiting for genesis. {GENESIS_HEIGHT - mine_height} blocks to go!")
            await asyncio.sleep(15)
            continue

        # Only attempt to mine the largest coin if multiple are found
        largest_cr = max(unspent_crs.coin_records, key=lambda r: r.coin.amount)
        for cr in [largest_cr]:

            # Skip if this coin was already submitted within its 3-block validity window
            # (puzzle enforces ASSERT_BEFORE_HEIGHT_ABSOLUTE (+ user_height 3))
            coin_id_key = cr.coin.coin_id()
            last_sub = submitted_coins.get(coin_id_key)
            if last_sub is not None and mine_height < last_sub + 3:
                continue
            epoch = get_epoch(mine_height)
            reward = get_reward(epoch)
            difficulty = get_difficulty(epoch)

            if cr.coin.amount < reward:
                print(f"Lode coin amount ({cr.coin.amount}) less than reward ({reward}), skipping")
                continue

            # Grind for a valid nonce
            nonce = find_valid_nonce(inner_puzzle_hash, pk_bytes, mine_height, difficulty)
            if nonce is None:
                print(f"Could not find valid nonce for height {mine_height}")
                continue

            #print(f"Found nonce {nonce} for coin {cr.coin.coin_id().hex()} at height {mine_height} (epoch {epoch}, reward {reward}, difficulty 2^{difficulty.bit_length()-1})")

            # Get parent spend to reconstruct CAT lineage
            parent_res = await client.get_coin_record_by_name(cr.coin.parent_coin_info)
            if not parent_res.success or parent_res.coin_record is None:
                print("Failed to get parent coin record")
                continue

            parent = parent_res.coin_record
            gps_res = await client.get_puzzle_and_solution(parent.coin.coin_id(), parent.spent_block_index)
            if not gps_res.success:
                print("Failed to get parent puzzle and solution")
                continue

            puzzle = CLVM.deserialize(gps_res.coin_solution.puzzle_reveal).puzzle()
            parent_solution = CLVM.deserialize(gps_res.coin_solution.solution)
            cats = puzzle.parse_child_cats(parent.coin, parent_solution)

            if cats is None:
                print("Failed to parse child CATs from parent")
                continue

            target_cat = None
            for cat in cats:
                if cat.info.p2_puzzle_hash == inner_puzzle_hash and cat.info.asset_id == CAT_TAIL_HASH:
                    target_cat = cat
                    break

            if target_cat is None:
                print("Could not find matching CAT child")
                continue

            # Build the inner puzzle solution
            # Solution: (my_amount my_inner_puzzlehash user_height miner_pubkey target_puzzle_hash nonce)
            solution_str = (
                f"({cr.coin.amount} "
                f"0x{inner_puzzle_hash.hex()} "
                f"{mine_height} "
                f"0x{pk_bytes.hex()} "
                f"0x{TARGET_PUZZLEHASH.hex()} "
                f"{nonce})"
            )

            inner_spend = Spend(curried_puzzle, CLVM.parse(solution_str))
            cat_spend = CatSpend(target_cat, inner_spend)
            CLVM.spend_cats([cat_spend])

            # Sign with AGG_SIG_ME
            # Message from puzzle: sha256(target_puzzle_hash + nonce + user_height)
            agg_sig_msg = pow_sha256(TARGET_PUZZLEHASH, nonce, mine_height)
            # Full signed message: msg + coin_id + genesis_challenge
            coin_id = cr.coin.coin_id()
            full_msg = agg_sig_msg + coin_id + genesis_challenge
            sig = sk.sign(full_msg)

            bundle = SpendBundle(
                CLVM.coin_spends(),
                sig,
            )

            try:
                tx_res = await client.push_tx(bundle)
                success = tx_res.success
                status_val = getattr(tx_res, "status", None)
                error = tx_res.error
            except Exception as e:
                print(f"Failed to push tx: {repr(e)}")
                continue

            if success:
                submitted_coins[coin_id_key] = mine_height
                # Prune stale entries beyond the 3-block window
                for k in [k for k, v in submitted_coins.items() if mine_height >= v + 3]:
                    del submitted_coins[k]
                print(
                    f"Submitted mining spend bundle for height {mine_height}, Status={status_val}"
                )
            else:
                print(f"Failed to submit mining spend bundle: {error}")

        await asyncio.sleep(15)


# ── Entry point ─────────────────────────────────────────────────────────

def main():
    banner = r"""
__   ___  __      _____   _ __ 
 \ \ / / | \ \    / / _ \ | '__|
  \ V /| | _\ \  / / (_) || |   
   > < | |/ /\ \/ / > _ < | |   
  / . \|   <  \  / | (_) || |   
 /_/ \_\_|\_\  \/   \___/ |_|
"""
    print(banner)
    print("Starting miner...")
    signal.signal(signal.SIGINT, sigint_handler)
    asyncio.run(mine())


def sigint_handler(_, __):
    print("\nGoodbye!")
    sys.exit(0)


if __name__ == "__main__":
    main()
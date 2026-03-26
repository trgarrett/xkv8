from __future__ import annotations

import asyncio
import hashlib
import json
import os
import signal
import sys
import urllib.error
import urllib.request
from typing import Optional

from clvm_tools_rs import compile as compile_clsp

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
from xkv8.utils import spend_bundle_to_json

# ── Puzzle parameters (production, testnet11) ────────────────────────────
CAT_TAIL_HASH = bytes.fromhex(
    "b0b56662d1a6732f0edbc0d428391b9250477042896e79135af4926e3ccac694"
)
GENESIS_HEIGHT = 3897519
EPOCH_LENGTH = 1_120_000
BASE_REWARD = 10_000  # mojos
BASE_DIFFICULTY = 2**238

# ── Environment ──────────────────────────────────────────────────────────
TARGET_ADDRESS = os.environ.get(
    "TARGET_ADDRESS",
    "txch12pfws6enm2jeqjt03pspqg6sjh50g86hl9xm24dx4cwwm2l88nmqwy99nd",
)
TARGET_PUZZLEHASH = Address.decode(TARGET_ADDRESS).puzzle_hash

# Miner secret key: 32-byte hex seed.  If absent a random key is created
# each run (fine for testing, but rewards go to TARGET_ADDRESS regardless).
_MINER_KEY_HEX = os.environ.get("MINER_SECRET_KEY", "")

# coin_id -> mine_height of last successful submission
submitted_coins: dict[bytes, int] = {}

CLVM = Clvm()


# ── Compile & curry the real puzzle ──────────────────────────────────────

def compile_puzzle() -> str:
    """Compile puzzle.clsp from source and return the hex string."""
    source = open("../clsp/puzzle.clsp").read()
    return compile_clsp(source, ["../clsp/include/"])


def build_curried_puzzle(clvm: Clvm):
    """Compile, curry, and return (curried_program, inner_puzzle_hash)."""
    hex_str = compile_puzzle()
    mod = clvm.deserialize(bytes.fromhex(hex_str))
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
    print(f"No MINER_SECRET_KEY set – generated ephemeral key (seed: {seed.hex()})")
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

COINSET_API_URL = "https://testnet11.api.coinset.org"

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
    client = RpcClient.testnet11()

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

    print(f"Inner puzzle hash: {inner_puzzle_hash.hex()}")
    print(f"CAT puzzle hash:   {full_cat_puzzlehash.hex()}")
    print(f"CAT TAIL hash:     {CAT_TAIL_HASH.hex()}")

    # Load miner key
    sk = load_miner_key()
    pk = sk.public_key()
    pk_bytes = pk.to_bytes()
    print(f"Miner public key:  {pk_bytes.hex()}")
    print(f"Mining to address:  {TARGET_ADDRESS}")
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
            await asyncio.sleep(15)
            continue

        for cr in unspent_crs.coin_records:

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
                payload = json.dumps(
                    {"spend_bundle": json.loads(spend_bundle_to_json(bundle))}
                ).encode()
                ##print(payload)
                req = urllib.request.Request(
                    f"{COINSET_API_URL}/push_tx",
                    data=payload,
                    headers={
                        "Content-Type": "application/json",
                        "Accept": "application/json",
                        "User-Agent": "xkv8-miner/1.0",
                    },
                    method="POST",
                )
                try:
                    with urllib.request.urlopen(req, timeout=30) as resp:
                        raw = resp.read()
                except urllib.error.HTTPError as http_err:
                    raw = http_err.read()
                body = json.loads(raw)
                success = body.get("success", False)
                status_val = body.get("status")
                error = body.get("error")
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
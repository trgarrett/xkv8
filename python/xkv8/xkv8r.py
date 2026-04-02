from __future__ import annotations

import asyncio
import concurrent.futures
import hashlib
import json
import random
import os
import pathlib
import signal
import sys
import threading
from typing import List, Optional

from chia_wallet_sdk import (
    Address,
    CatSpend,
    Clvm,
    RpcClient,
    Constants,
    SecretKey,
    Signature,
    Spend,
    SpendBundle,
    cat_puzzle_hash,
    select_coins,
    standard_puzzle_hash,
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

THREAD_COUNT = int(os.environ.get("THREAD_COUNT", "1"))

# FEE_MOJOS: optional fee (in mojos) to attach to each mining spend bundle.
# When > 0, the miner will look for XCH coins at the standard transaction
# address derived from MINER_SECRET_KEY and attach a fee coin spend.
FEE_MOJOS = int(os.environ.get("FEE_MOJOS", "0"))

# DEBUG: set to "1" to log the full JSON representation of every spend
# bundle (including fee spends) before it is pushed to the network.
DEBUG = os.environ.get("DEBUG", "0") == "1"

# LOCAL_FULL_NODE: when set to any truthy value (e.g. "1" or "host:port"),
# a native RPC client using TLS certs is created as the primary client.
# If the value contains a colon it is treated as host:port; otherwise the
# default https://localhost:8555 is used.
LOCAL_FULL_NODE = os.environ.get("LOCAL_FULL_NODE", None)

NETWORK_NAME="mainnet"

TESTNET = os.environ.get("TESTNET", None)

if TESTNET is not None:
    NETWORK_NAME="testnet11"

# CHIA_ROOT: path to the Chia data directory (default: ~/.chia/mainnet).
# The full-node TLS certs are read from $CHIA_ROOT/config/ssl/full_node/.
CHIA_ROOT = pathlib.Path(
    os.environ.get("CHIA_ROOT", pathlib.Path.home() / ".chia" / NETWORK_NAME)
)

if TESTNET is None and not TARGET_ADDRESS.startswith("xch"):
    print("Error: TARGET_ADDRESS must be a mainnet address (starting with 'xch')")
    sys.exit(1)
elif TESTNET is not None and not TARGET_ADDRESS.startswith("txch"):
    print("Error: TARGET_ADDRESS must be a testnet address (starting with 'txch')")
    sys.exit(1)

DEFAULT_SLEEP=float(os.environ.get("DEFAULT_SLEEP", "5"))
ERROR_SLEEP=2

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


def _search_nonce_range(
    inner_puzzle_hash: bytes,
    miner_pubkey_bytes: bytes,
    h_bytes: bytes,
    difficulty: int,
    start: int,
    end: int,
    found_event: threading.Event,
) -> Optional[int]:
    """Search a slice of the nonce space. Returns a valid nonce or None."""
    for nonce in range(start, end):
        if found_event.is_set():
            return None
        n_bytes = int_to_clvm_bytes(nonce)
        digest = hashlib.sha256(
            inner_puzzle_hash + miner_pubkey_bytes + h_bytes + n_bytes
        ).digest()
        pow_int = int.from_bytes(digest, "big")
        if pow_int > 0 and difficulty > pow_int:
            found_event.set()
            return nonce
    return None


def find_valid_nonce(
    inner_puzzle_hash: bytes,
    miner_pubkey_bytes: bytes,
    user_height: int,
    difficulty: int,
    max_attempts: int = 5_000_000,
) -> Optional[int]:
    """Grind for a nonce that satisfies the PoW target using up to THREAD_COUNT threads."""
    h_bytes = int_to_clvm_bytes(user_height)

    if THREAD_COUNT <= 1:
        # Single-threaded fast path
        for nonce in range(max_attempts):
            n_bytes = int_to_clvm_bytes(nonce)
            digest = hashlib.sha256(
                inner_puzzle_hash + miner_pubkey_bytes + h_bytes + n_bytes
            ).digest()
            pow_int = int.from_bytes(digest, "big")
            if pow_int > 0 and difficulty > pow_int:
                return nonce
        return None

    # Multi-threaded path
    found_event = threading.Event()
    chunk_size = (max_attempts + THREAD_COUNT - 1) // THREAD_COUNT

    with concurrent.futures.ThreadPoolExecutor(max_workers=THREAD_COUNT) as executor:
        futures = []
        for i in range(THREAD_COUNT):
            start = i * chunk_size
            end = min(start + chunk_size, max_attempts)
            if start >= max_attempts:
                break
            futures.append(
                executor.submit(
                    _search_nonce_range,
                    inner_puzzle_hash,
                    miner_pubkey_bytes,
                    h_bytes,
                    difficulty,
                    start,
                    end,
                    found_event,
                )
            )

        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            if result is not None:
                # Cancel remaining futures and return the found nonce
                found_event.set()
                return result

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


def _load_full_node_certs() -> tuple[bytes, bytes]:
    """Read the private full-node TLS cert and key from CHIA_ROOT."""
    ssl_dir = CHIA_ROOT / "config" / "ssl" / "full_node"
    cert_path = ssl_dir / "private_full_node.crt"
    key_path = ssl_dir / "private_full_node.key"
    if not cert_path.exists() or not key_path.exists():
        print(f"Error: Could not find full-node TLS certs in {ssl_dir}")
        print("  Ensure your Chia node is set up, or set CHIA_ROOT to the correct directory.")
        sys.exit(1)
    return cert_path.read_bytes(), key_path.read_bytes()


def build_clients() -> List[RpcClient]:
    """Build the ordered list of RPC clients.

    * If LOCAL_FULL_NODE is set, an RpcClient using native TLS-authenticated
      RPC (via RpcClient.local / RpcClient.local_with_url) is placed at
      index 0 (primary / sync client).
    * The public coinset endpoint (mainnet or testnet11) is always
      included as the fallback (or sole) client.
    """
    clients: List[RpcClient] = []

    if LOCAL_FULL_NODE is not None:
        cert_bytes, key_bytes = _load_full_node_certs()
        # If the value looks like host:port or a full URL, use local_with_url;
        # otherwise use the default local() which targets https://localhost:8555.
        if ":" in LOCAL_FULL_NODE:
            url = LOCAL_FULL_NODE if LOCAL_FULL_NODE.startswith("http") else f"https://{LOCAL_FULL_NODE}"
            print(f"Using local full node RPC at {url} (native TLS)")
            clients.append(RpcClient.local_with_url(url, cert_bytes, key_bytes))
        else:
            print("Using local full node RPC at https://localhost:8555 (native TLS)")
            clients.append(RpcClient.local(cert_bytes, key_bytes))

    if TESTNET is not None:
        clients.append(RpcClient.testnet11())
    else:
        clients.append(RpcClient.mainnet())

    return clients


async def push_tx_to_all(clients: List[RpcClient], bundle: SpendBundle):
    """Push a spend bundle to every client concurrently.

    Returns (success, status, error) from the *first* client that succeeds,
    or the result from the primary (0th) client if none succeed.
    """
    async def _push(client: RpcClient):
        return await client.push_tx(bundle)

    results = await asyncio.gather(*[_push(c) for c in clients], return_exceptions=True)

    # Prefer the first successful result; fall back to the primary client's result
    primary_result = results[0]
    for res in results:
        if isinstance(res, BaseException):
            continue
        tx_res = res  # type: ignore[union-attr]
        if tx_res.success:
            return tx_res.success, getattr(tx_res, "status", None), tx_res.error
    # No success – return primary result (may itself be an exception)
    if isinstance(primary_result, BaseException):
        raise primary_result
    tx_primary = primary_result  # type: ignore[union-attr]
    return tx_primary.success, getattr(tx_primary, "status", None), tx_primary.error


async def mine():
    clients = build_clients()
    client = clients[0]  # primary client for syncing data

    if TESTNET is not None:
        genesis_challenge = GENESIS_CHALLENGES["testnet11"]
    else:
        genesis_challenge = GENESIS_CHALLENGES["mainnet"]

    #print(f"Genesis challenge: {genesis_challenge.hex()}")

    # Compile & curry the puzzle once
    curried_puzzle, inner_puzzle_hash, cat_mod_hash = build_curried_puzzle(CLVM)
    full_cat_puzzlehash = cat_puzzle_hash(CAT_TAIL_HASH, inner_puzzle_hash)

    print(f"Lode puzzle hash: {inner_puzzle_hash.hex()}")
    print(f"Lode full CAT puzzle hash: {full_cat_puzzlehash.hex()}")
    if THREAD_COUNT > 1:
        print(f"Mining with up to {THREAD_COUNT} threads for nonce grinding")
    #print(f"CAT TAIL hash:     {CAT_TAIL_HASH.hex()}")

    # Load miner key
    sk = load_miner_key()
    pk = sk.public_key()
    pk_bytes = pk.to_bytes()
    print(f"Miner public key: {pk_bytes.hex()}")
    print(f"Mining to address: {TARGET_ADDRESS}")

    # Derive the standard transaction (p2_delegated_puzzle_or_hidden) address
    # for fee coin management.
    synthetic_sk = sk.derive_synthetic()
    synthetic_pk = synthetic_sk.public_key()
    fee_puzzlehash = standard_puzzle_hash(synthetic_pk)
    fee_prefix = "txch" if TESTNET is not None else "xch"
    fee_address = Address(fee_puzzlehash, fee_prefix).encode()
    if FEE_MOJOS > 0:
        print(f"Fee mode: {FEE_MOJOS} mojos per spend")
        print(f"Fee address: {fee_address}")
        print(f"  → Send XCH to this address to enable fee-boosted mining")
    else:
        print(f"Fee address (not active, set FEE_MOJOS to enable): {fee_address}")
    print()

    last_height = -1

    while True:
        try:
            blockchain_state = None
            for c in clients:
                try:
                    res = await c.get_blockchain_state()
                    if res.success:
                        client = c
                        blockchain_state = res
                        break
                except Exception:
                    continue
            if blockchain_state is None:
                print("Failed to get blockchain state from any client")
                await asyncio.sleep(ERROR_SLEEP * (0.5 + random.random()))
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
            # (may not be indexed on a local node; fall back through clients)
            unspent_crs = None
            for c in clients:
                try:
                    res = await c.get_coin_records_by_puzzle_hash(
                        full_cat_puzzlehash, GENESIS_HEIGHT, height + 5, False,
                    )
                    if res.success:
                        unspent_crs = res
                        break
                    else:
                        print(f"get_coin_records_by_puzzle_hash failed: success=false, error={res.error!r}, coin_records={res.coin_records!r}")
                except Exception as e:
                    print(f"get_coin_records_by_puzzle_hash exception: {repr(e)}")
            if unspent_crs is None or not unspent_crs.success:
                print("Failed to discover unspent coins on any client")
                await asyncio.sleep(ERROR_SLEEP * (0.5 + random.random()))
                continue

            # Don't attempt mining before genesis height
            mine_height = 1 + last_height
            if mine_height < GENESIS_HEIGHT:
                print(f"Waiting for genesis. {GENESIS_HEIGHT - mine_height} blocks to go!")
                await asyncio.sleep(DEFAULT_SLEEP)
                continue

            # Only attempt to mine the largest coin if multiple are found
            if not unspent_crs.coin_records:
                continue

            # Prefer the most recently confirmed coin, but avoid coins
            # whose amount is notably lower than the richest one.
            max_amount = max(r.coin.amount for r in unspent_crs.coin_records)
            viable = [r for r in unspent_crs.coin_records if r.coin.amount >= max_amount * 0.9]
            largest_cr = max(viable, key=lambda r: r.confirmed_block_index)
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

                # ── Optional fee coin attachment ─────────────────────────
                # If FEE_MOJOS > 0, look for spendable XCH at the standard
                # transaction address derived from the miner key.  The fee
                # coin asserts the mining coin's announcement (CREATE_COIN_
                # ANNOUNCEMENT '$') so the two spends are atomically bound.
                fee_sig = Signature.infinity()
                if FEE_MOJOS > 0:
                    fee_coins_found = False
                    # Step 1: discover fee coins from any available client
                    available_coins: list = []
                    for c in clients:
                        try:
                            fee_res = await c.get_coin_records_by_puzzle_hash(
                                fee_puzzlehash, GENESIS_HEIGHT, height + 5, False,
                            )
                            if fee_res.success and fee_res.coin_records:
                                available_coins = [r.coin for r in fee_res.coin_records]
                                break
                        except Exception as e:
                            print(f"Fee coin lookup failed on client: {repr(e)}")
                            continue

                    # Step 2: select coins and build the fee spend
                    if available_coins:
                        try:
                            selected = select_coins(available_coins, FEE_MOJOS)
                        except Exception as e:
                            print(f"Fee coin selection failed: {repr(e)}")
                            selected = []

                        if selected:
                            try:
                                fee_coins_found = True
                                total_in = sum(fc.amount for fc in selected)
                                change = total_in - FEE_MOJOS

                                # Announcement ID = sha256(mining_coin_id || '$')
                                announcement_id = hashlib.sha256(
                                    coin_id + b'$'
                                ).digest()

                                # Conditions for the primary fee coin
                                conditions = [
                                    CLVM.assert_coin_announcement(announcement_id),
                                    CLVM.reserve_fee(FEE_MOJOS),
                                ]
                                if change > 0:
                                    conditions.append(
                                        CLVM.create_coin(fee_puzzlehash, change, CLVM.list([CLVM.atom(fee_puzzlehash)]))
                                    )

                                delegated = CLVM.delegated_spend(conditions)
                                CLVM.spend_standard_coin(
                                    selected[0], synthetic_pk, delegated
                                )
                                dpuz_hash = delegated.puzzle.tree_hash()
                                fee_sigs = [
                                    synthetic_sk.sign(
                                        dpuz_hash
                                        + selected[0].coin_id()
                                        + genesis_challenge
                                    )
                                ]

                                # Extra selected coins: empty delegated spend
                                for extra in selected[1:]:
                                    empty_del = CLVM.delegated_spend([])
                                    CLVM.spend_standard_coin(
                                        extra, synthetic_pk, empty_del
                                    )
                                    fee_sigs.append(
                                        synthetic_sk.sign(
                                            empty_del.puzzle.tree_hash()
                                            + extra.coin_id()
                                            + genesis_challenge
                                        )
                                    )

                                fee_sig = Signature.aggregate(fee_sigs)
                                print(f"Attached fee of {FEE_MOJOS} mojos ({len(selected)} coin(s), change={change})")
                            except Exception as e:
                                print(f"Error building fee spend: {repr(e)}")
                                fee_coins_found = False

                    if not fee_coins_found:
                        print(
                            f"Warning: FEE_MOJOS={FEE_MOJOS} but no spendable "
                            f"coins at {fee_address} – submitting without fee"
                        )

                bundle = SpendBundle(
                    CLVM.coin_spends(),
                    Signature.aggregate([sig, fee_sig]),
                )

                if DEBUG:
                    coin_spends_json = []
                    for cs in bundle.coin_spends:
                        coin_spends_json.append({
                            "coin": {
                                "parent_coin_info": cs.coin.parent_coin_info.hex(),
                                "puzzle_hash": cs.coin.puzzle_hash.hex(),
                                "amount": cs.coin.amount,
                            },
                            "puzzle_reveal": cs.puzzle_reveal.hex(),
                            "solution": cs.solution.hex(),
                        })
                    bundle_json = {
                        "coin_spends": coin_spends_json,
                        "aggregated_signature": bundle.aggregated_signature.to_bytes().hex(),
                    }
                    print("[DEBUG] Spend bundle JSON:")
                    print(json.dumps(bundle_json, indent=2))

                try:
                    success, status_val, error = await push_tx_to_all(clients, bundle)
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
            await asyncio.sleep(DEFAULT_SLEEP)
        except Exception as e:
            print(f"Error in mining loop: {repr(e)}")
            await asyncio.sleep(ERROR_SLEEP * (0.5 + random.random()))


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

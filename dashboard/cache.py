"""
XKV8 Mining Cache

Maintains a local filesystem cache of mined coins (spent lode coins) and
extracts miner pubkeys from their solutions.  Always re-downloads the last
N coins (by spent height) to handle potential re-orgs.

Cache file: dashboard/.mine_cache.json
"""

import json
import os
from pathlib import Path
from typing import Optional

from chia_wallet_sdk import Clvm, RpcClient

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

REORG_WINDOW = 20  # always re-fetch the most recent N spends
CACHE_FILE = Path(__file__).resolve().parent / ".mine_cache.json"

if os.environ.get("TESTNET"):
    FULL_CAT_PUZZLE_HASH = bytes.fromhex("1a6e78906757f302d0c50b77cad94a59d64298014a5691f50cd19535c61d5d02")
else:
    FULL_CAT_PUZZLE_HASH = bytes.fromhex("e758f3dba6baac1a6e581ce46537811157621986e18c350075948049abc479f1")

GENESIS_HEIGHT = 8_521_888
EPOCH_LENGTH = 1_120_000
BASE_REWARD = 10_000


# ---------------------------------------------------------------------------
# Epoch / reward helpers (must match puzzle.clsp)
# ---------------------------------------------------------------------------

def get_epoch(block_height: int) -> int:
    if block_height <= GENESIS_HEIGHT:
        return 0
    raw = (block_height - GENESIS_HEIGHT) // EPOCH_LENGTH
    return min(raw, 3)


def get_reward(epoch: int) -> int:
    return BASE_REWARD >> epoch


# ---------------------------------------------------------------------------
# Cache I/O
# ---------------------------------------------------------------------------

def _load_cache() -> dict:
    """Load the cache from disk, returning an empty structure if missing."""
    if CACHE_FILE.exists():
        try:
            with open(CACHE_FILE, "r") as f:
                return json.load(f)
        except (json.JSONDecodeError, OSError):
            pass
    return {"coins": {}}


async def load_cache(client: RpcClient) -> dict:
    """Load the cache from disk; if it contains no coins, refresh from chain."""
    cache = _load_cache()
    if not cache.get("coins"):
        cache = await refresh_cache(client)
    return cache


def _save_cache(cache: dict) -> None:
    """Persist the cache to disk."""
    CACHE_FILE.parent.mkdir(parents=True, exist_ok=True)
    with open(CACHE_FILE, "w") as f:
        json.dump(cache, f, indent=2)


# ---------------------------------------------------------------------------
# Solution parsing
# ---------------------------------------------------------------------------

def _extract_miner_pubkey(clvm: Clvm, solution_bytes: bytes) -> Optional[str]:
    """
    Extract the miner pubkey from a CAT-wrapped solution.

    CAT v2 solution structure:  (inner_solution ...)
    Inner solution structure:   (my_amount my_inner_puzzlehash user_height
                                 miner_pubkey target_puzzle_hash nonce)

    Returns the miner pubkey as a hex string, or None on failure.
    """
    try:
        full_solution = clvm.deserialize(solution_bytes)
        inner_solution = full_solution.first()
        # Navigate: rest() -> skip my_amount
        #           rest() -> skip my_inner_puzzlehash
        #           rest() -> skip user_height
        #           first() -> miner_pubkey
        miner_pubkey_program = inner_solution.rest().rest().rest().first()
        pubkey_bytes = miner_pubkey_program.to_atom()
        if pubkey_bytes is None:
            return None
        return pubkey_bytes.hex()
    except Exception:
        return None


def _extract_user_height(clvm: Clvm, solution_bytes: bytes) -> Optional[int]:
    """
    Extract user_height from a CAT-wrapped solution.
    """
    try:
        full_solution = clvm.deserialize(solution_bytes)
        inner_solution = full_solution.first()
        # rest() -> skip my_amount
        # rest() -> skip my_inner_puzzlehash
        # first() -> user_height
        height_program = inner_solution.rest().rest().first()
        return height_program.to_int()
    except Exception:
        return None


# ---------------------------------------------------------------------------
# Main cache refresh
# ---------------------------------------------------------------------------

async def refresh_cache(client: RpcClient) -> dict:
    """
    Refresh the local mine cache and return the full cache dict.

    Strategy:
      1. Load existing cache from disk.
      2. Fetch all spent coin records for FULL_CAT_PUZZLE_HASH.
      3. Determine which coins need (re-)fetching:
         - Any coin not yet in the cache.
         - The most recent REORG_WINDOW coins by spent_height (re-org safety).
      4. For each coin to fetch, call get_puzzle_and_solution to extract
         the miner pubkey from the REMARK in the solution.
      5. Remove any cached coins that no longer appear in the on-chain
         spent set (they were re-orged away).
      6. Save and return the updated cache.
    """
    cache = _load_cache()
    cached_coins: dict = cache.get("coins", {})

    # Fetch all coin records (spent + unspent) for the lode puzzle hash
    response = await client.get_coin_records_by_puzzle_hash(
        FULL_CAT_PUZZLE_HASH, None, None, True,
    )
    if not response.success or response.coin_records is None:
        print("[cache] Failed to fetch coin records, using stale cache")
        return cache

    # Filter to only spent coins (these are the mines)
    spent_records = [cr for cr in response.coin_records if cr.spent]

    # Sort by spent_block_index ascending
    spent_records.sort(key=lambda cr: cr.spent_block_index)

    # Build set of on-chain coin IDs for pruning
    on_chain_ids = {cr.coin.coin_id().hex() for cr in spent_records}

    # Prune cached coins that are no longer on-chain (re-orged)
    stale_ids = [cid for cid in cached_coins if cid not in on_chain_ids]
    for cid in stale_ids:
        del cached_coins[cid]
    if stale_ids:
        print(f"[cache] Pruned {len(stale_ids)} re-orged coin(s)")

    # Determine the reorg window: the last REORG_WINDOW spent coins
    reorg_cutoff_ids = set()
    if len(spent_records) > 0:
        tail = spent_records[-REORG_WINDOW:]
        reorg_cutoff_ids = {cr.coin.coin_id().hex() for cr in tail}

    # Identify coins that need fetching
    to_fetch = []
    for cr in spent_records:
        coin_id_hex = cr.coin.coin_id().hex()
        if coin_id_hex not in cached_coins or coin_id_hex in reorg_cutoff_ids:
            to_fetch.append(cr)

    if to_fetch:
        print(f"[cache] Fetching solutions for {len(to_fetch)} coin(s)...")

    clvm = Clvm()
    fetched = 0
    for cr in to_fetch:
        coin_id = cr.coin.coin_id()
        coin_id_hex = coin_id.hex()
        try:
            gps = await client.get_puzzle_and_solution(
                coin_id, cr.spent_block_index,
            )
            if not gps.success or gps.coin_solution is None:
                print(f"[cache] Could not get solution for {coin_id_hex[:16]}…")
                continue

            miner_pubkey = _extract_miner_pubkey(clvm, gps.coin_solution.solution)
            user_height = _extract_user_height(clvm, gps.coin_solution.solution)

            if miner_pubkey is None:
                print(f"[cache] Could not parse miner pubkey for {coin_id_hex[:16]}…")
                continue

            # Compute reward from user_height (most accurate) or spent_block_index
            height_for_reward = user_height if user_height is not None else cr.spent_block_index
            epoch = get_epoch(height_for_reward)
            reward = get_reward(epoch)

            cached_coins[coin_id_hex] = {
                "miner_pubkey": miner_pubkey,
                "reward": reward,
                "spent_height": cr.spent_block_index,
                "coin_amount": cr.coin.amount,
            }
            fetched += 1
        except Exception as e:
            print(f"[cache] Error fetching {coin_id_hex[:16]}…: {e!r}")

    if fetched:
        print(f"[cache] Fetched {fetched} new/updated solution(s)")

    cache["coins"] = cached_coins
    _save_cache(cache)
    return cache


# ---------------------------------------------------------------------------
# Leaderboard aggregation
# ---------------------------------------------------------------------------

def build_recent_wins(cache: dict, count: int = 20) -> list[dict]:
    """
    Return the most recent mines from the cache, sorted by spent_height
    descending (newest first).

    Returns a list of dicts with keys:
      - pubkey: hex string of the miner's BLS public key
      - reward: reward in mojos
      - spent_height: block height the coin was spent at
    """
    coins = cache.get("coins", {})

    entries = []
    for entry in coins.values():
        pk = entry.get("miner_pubkey")
        if pk is None:
            continue
        entries.append({
            "pubkey": pk,
            "reward": entry.get("reward", 0),
            "spent_height": entry.get("spent_height", 0),
        })

    entries.sort(key=lambda e: e["spent_height"], reverse=True)
    return entries[:count]


def build_leaderboard(cache: dict, count: int = 50) -> list[dict]:
    """
    Aggregate cached mine data into a leaderboard sorted by total mined
    (descending).

    Returns a list of dicts with keys:
      - pubkey: hex string of the miner's BLS public key
      - total_mined: total reward mojos earned
      - blocks_won: number of successful mines
    """
    coins = cache.get("coins", {})

    # Aggregate by miner pubkey
    agg: dict[str, dict] = {}
    for entry in coins.values():
        pk = entry.get("miner_pubkey")
        if pk is None:
            continue
        if pk not in agg:
            agg[pk] = {"total_mined": 0, "blocks_won": 0}
        agg[pk]["total_mined"] += entry.get("reward", 0)
        agg[pk]["blocks_won"] += 1

    # Sort descending by total_mined, then by blocks_won as tiebreaker
    leaderboard = []
    for pubkey, stats in agg.items():
        leaderboard.append({
            "pubkey": pubkey,
            "total_mined": stats["total_mined"],
            "blocks_won": stats["blocks_won"],
        })
    leaderboard.sort(key=lambda m: (m["total_mined"], m["blocks_won"]), reverse=True)

    return leaderboard[:count]
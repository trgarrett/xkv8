#!/usr/bin/env python3
"""
Simulate find_valid_nonce for a specific pubkey to determine whether it
has an easier time finding valid nonces compared to a random pubkey.

This script extracts the pure PoW logic from xkv8r.py (no network, no
Chia wallet SDK needed beyond Clvm for the one-time puzzle hash derivation).
"""

from __future__ import annotations

import hashlib
import os
import statistics
import time
from typing import Optional

# ---------------------------------------------------------------------------
# Replicated PoW helpers from xkv8r.py
# ---------------------------------------------------------------------------

CAT_TAIL_HASH = bytes.fromhex(
    "f09c8d630a0a64eb4633c0933e0ca131e646cebb384cfc4f6718bad80859b5e8"
)
GENESIS_HEIGHT = 8521888
EPOCH_LENGTH   = 1_120_000
BASE_REWARD    = 10_000
BASE_DIFFICULTY = 2**238

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


def int_to_clvm_bytes(n: int) -> bytes:
    """Encode a Python int as CLVM-style signed big-endian bytes."""
    if n == 0:
        return b""
    byte_len = (n.bit_length() + 8) // 8
    return n.to_bytes(byte_len, "big", signed=True)


def get_epoch(user_height: int) -> int:
    raw = (user_height - GENESIS_HEIGHT) // EPOCH_LENGTH
    return min(raw, 3)


def get_difficulty(epoch: int) -> int:
    return BASE_DIFFICULTY >> epoch


def find_valid_nonce(
    inner_puzzle_hash: bytes,
    miner_pubkey_bytes: bytes,
    user_height: int,
    difficulty: int,
    max_attempts: int = 5_000_000,
) -> Optional[int]:
    """Grind for a nonce that satisfies the PoW target (single-threaded)."""
    h_bytes = int_to_clvm_bytes(user_height)
    for nonce in range(max_attempts):
        n_bytes = int_to_clvm_bytes(nonce)
        digest = hashlib.sha256(
            inner_puzzle_hash + miner_pubkey_bytes + h_bytes + n_bytes
        ).digest()
        pow_int = int.from_bytes(digest, "big")
        if pow_int > 0 and difficulty > pow_int:
            return nonce
    return None


# ---------------------------------------------------------------------------
# Build inner_puzzle_hash using chia_wallet_sdk
# ---------------------------------------------------------------------------

def build_inner_puzzle_hash() -> bytes:
    """Curry the compiled puzzle and return the inner puzzle hash."""
    from chia_wallet_sdk import Clvm, Constants

    clvm = Clvm()
    mod = clvm.deserialize(bytes.fromhex(PUZZLE_HEX))
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
    return curried.tree_hash()


# ---------------------------------------------------------------------------
# Simulation
# ---------------------------------------------------------------------------

TARGET_PUBKEY_HEX = (
    "954ec7a9eee404b4fb4f381545d90abcdcab6880dea240f88a4b1f1aacc3a70f"
    "ba9c96ae5aab14b9e6d09c51e25493ad"
)

# Generate a deterministic "random" comparison pubkey (48 bytes)
RANDOM_PUBKEY = hashlib.sha256(b"random-comparison-key").digest() + \
                hashlib.sha256(b"random-comparison-key-part2").digest()[:16]

NUM_HEIGHTS = 20          # how many heights to test per pubkey
START_HEIGHT = 8_522_000  # a height in epoch 0, shortly after genesis


def simulate_pubkey(
    label: str,
    pubkey_bytes: bytes,
    inner_puzzle_hash: bytes,
    heights: list[int],
) -> list[dict]:
    """Run find_valid_nonce for each height, collecting nonce value and timing."""
    results = []
    for h in heights:
        epoch = get_epoch(h)
        difficulty = get_difficulty(epoch)

        t0 = time.perf_counter()
        nonce = find_valid_nonce(inner_puzzle_hash, pubkey_bytes, h, difficulty)
        elapsed = time.perf_counter() - t0

        results.append({
            "height": h,
            "nonce": nonce,
            "elapsed_s": elapsed,
            "epoch": epoch,
            "difficulty_bits": difficulty.bit_length() - 1,
        })
        status = f"nonce={nonce}" if nonce is not None else "FAILED"
        print(f"  [{label}] height={h}  {status}  time={elapsed:.4f}s")
    return results


def print_summary(label: str, results: list[dict]) -> None:
    found = [r for r in results if r["nonce"] is not None]
    nonces = [r["nonce"] for r in found]
    times = [r["elapsed_s"] for r in found]

    print(f"\n{'='*60}")
    print(f"  {label}")
    print(f"{'='*60}")
    print(f"  Heights tested       : {len(results)}")
    print(f"  Nonces found         : {len(found)} / {len(results)}")
    if nonces:
        print(f"  Nonce min            : {min(nonces):,}")
        print(f"  Nonce max            : {max(nonces):,}")
        print(f"  Nonce mean           : {statistics.mean(nonces):,.1f}")
        print(f"  Nonce median         : {statistics.median(nonces):,.1f}")
    if times:
        print(f"  Time  min            : {min(times):.4f}s")
        print(f"  Time  max            : {max(times):.4f}s")
        print(f"  Time  mean           : {statistics.mean(times):.4f}s")
        print(f"  Time  median         : {statistics.median(times):.4f}s")
        print(f"  Time  total          : {sum(times):.4f}s")
    print()


def main():
    print("Building inner_puzzle_hash (requires chia_wallet_sdk)...")
    inner_puzzle_hash = build_inner_puzzle_hash()
    print(f"Inner puzzle hash: {inner_puzzle_hash.hex()}\n")

    target_pubkey = bytes.fromhex(TARGET_PUBKEY_HEX)
    random_pubkey = RANDOM_PUBKEY

    print(f"Target pubkey : {target_pubkey.hex()}")
    print(f"Random pubkey : {random_pubkey.hex()}")
    print(f"Heights       : {START_HEIGHT} .. {START_HEIGHT + NUM_HEIGHTS - 1}")

    epoch = get_epoch(START_HEIGHT)
    difficulty = get_difficulty(epoch)
    print(f"Epoch         : {epoch}")
    print(f"Difficulty    : 2^{difficulty.bit_length() - 1}")
    print(f"Max nonce     : 5,000,000")
    print()

    # ── PoW distribution analysis ────────────────────────────────────────
    # For a single height, sample many nonces and see how the hash values
    # distribute.  A "lucky" pubkey would shift the distribution lower.
    sample_height = START_HEIGHT
    h_bytes = int_to_clvm_bytes(sample_height)
    sample_n = 10_000

    print(f"--- Hash-value distribution sample (first {sample_n:,} nonces, height={sample_height}) ---")
    for label, pk in [("Target", target_pubkey), ("Random", random_pubkey)]:
        vals = []
        for nonce in range(sample_n):
            n_bytes = int_to_clvm_bytes(nonce)
            digest = hashlib.sha256(
                inner_puzzle_hash + pk + h_bytes + n_bytes
            ).digest()
            vals.append(int.from_bytes(digest, "big"))
        mean_val = statistics.mean(vals)
        min_val = min(vals)
        hits = sum(1 for v in vals if v > 0 and difficulty > v)
        print(f"  [{label}] hits_under_difficulty={hits}  min_hash_bits={min_val.bit_length()}  mean_hash_bits={int(mean_val).bit_length()}")
    print()

    # ── Nonce-finding benchmark across multiple heights ──────────────────
    heights = list(range(START_HEIGHT, START_HEIGHT + NUM_HEIGHTS))

    print(f"--- Benchmarking target pubkey across {NUM_HEIGHTS} heights ---")
    target_results = simulate_pubkey("Target", target_pubkey, inner_puzzle_hash, heights)

    print(f"\n--- Benchmarking random pubkey across {NUM_HEIGHTS} heights ---")
    random_results = simulate_pubkey("Random", random_pubkey, inner_puzzle_hash, heights)

    # ── Summary ──────────────────────────────────────────────────────────
    print_summary("Target Pubkey  " + TARGET_PUBKEY_HEX[:32] + "...", target_results)
    print_summary("Random Pubkey  " + random_pubkey.hex()[:32] + "...", random_results)

    # ── Verdict ──────────────────────────────────────────────────────────
    target_nonces = [r["nonce"] for r in target_results if r["nonce"] is not None]
    random_nonces = [r["nonce"] for r in random_results if r["nonce"] is not None]

    if target_nonces and random_nonces:
        t_mean = statistics.mean(target_nonces)
        r_mean = statistics.mean(random_nonces)
        t_time = statistics.mean([r["elapsed_s"] for r in target_results if r["nonce"] is not None])
        r_time = statistics.mean([r["elapsed_s"] for r in random_results if r["nonce"] is not None])
        ratio = r_mean / t_mean if t_mean > 0 else float("inf")
        time_ratio = r_time / t_time if t_time > 0 else float("inf")
        print("=" * 60)
        print("  VERDICT")
        print("=" * 60)
        print(f"  Mean nonce (target) : {t_mean:,.1f}")
        print(f"  Mean nonce (random) : {r_mean:,.1f}")
        print(f"  Nonce ratio (random/target): {ratio:.2f}x")
        print(f"  Mean time  (target) : {t_time:.4f}s")
        print(f"  Mean time  (random) : {r_time:.4f}s")
        print(f"  Time ratio (random/target) : {time_ratio:.2f}x")
        print()
        if ratio > 1.5:
            print("  ⚡ The target pubkey finds nonces SIGNIFICANTLY faster (lower nonces).")
        elif ratio > 1.1:
            print("  ✅ The target pubkey finds nonces somewhat faster.")
        elif ratio > 0.9:
            print("  ≈  Both pubkeys find nonces at roughly the same rate.")
        elif ratio > 0.67:
            print("  ❌ The target pubkey is somewhat SLOWER at finding nonces.")
        else:
            print("  ❌ The target pubkey is SIGNIFICANTLY SLOWER at finding nonces.")
        print()
        print("  NOTE: SHA-256 is a pseudorandom function. Any pubkey-dependent")
        print("  advantage is purely coincidental for these specific heights and")
        print("  does NOT generalize. Over enough heights the expected nonce is")
        print("  ~equal for all pubkeys (≈ 2^256 / difficulty).")
    else:
        print("Insufficient data for comparison (one or both pubkeys failed to find nonces).")


if __name__ == "__main__":
    main()

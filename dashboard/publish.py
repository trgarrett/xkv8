#!/usr/bin/env python3
"""
XKV8 Dashboard Publisher

Renders the dashboard template with current data and writes static HTML.
Can run once or continuously on an interval.

Usage:
    python publish.py                  # Run once
    python publish.py --watch          # Run every 60 seconds
    python publish.py --watch --interval 30  # Run every 30 seconds
"""

import argparse
import asyncio
import os
import signal
import sys
from datetime import datetime, timezone
from pathlib import Path

from chia_wallet_sdk import RpcClient
from jinja2 import Environment, FileSystemLoader

from cache import refresh_cache, build_leaderboard, build_recent_wins

if os.environ.get("TESTNET"):
    FULL_CAT_PUZZLE_HASH = bytes.fromhex("1a6e78906757f302d0c50b77cad94a59d64298014a5691f50cd19535c61d5d02")
    client = RpcClient.testnet11()
else:
    FULL_CAT_PUZZLE_HASH = bytes.fromhex("e758f3dba6baac1a6e581ce46537811157621986e18c350075948049abc479f1")
    client = RpcClient.mainnet()

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _format_number(n) -> str:
    if isinstance(n, float):
        if n == int(n):
            return f"{int(n):,}"
        return f"{n:,.3f}"
    return f"{n:,}"


# ---------------------------------------------------------------------------
# Epoch / reward constants (must match clsp/puzzle.clsp curried parameters)
# ---------------------------------------------------------------------------
GENESIS_HEIGHT = 8_521_888
EPOCH_LENGTH = 1_120_000  # blocks per epoch (~8 months)
BASE_REWARD = 10_000  # initial reward per mine in mojos

async def get_current_block_height() -> int:
    blockchain_state = await client.get_blockchain_state()
    if not blockchain_state.success:
        print("Failed to get blockchain state")
        raise Exception("RPC error")
    return blockchain_state.blockchain_state.peak.height


async def get_total_mined() -> float:
    unspent_crs = await client.get_coin_records_by_puzzle_hash(
        FULL_CAT_PUZZLE_HASH, None, None, False,
    )
    largest_cr = max(unspent_crs.coin_records, key=lambda r: r.coin.amount)
    return 21000000.000 - (largest_cr.coin.amount / 1000)


def get_epoch(block_height: int) -> int:
    """Calculate the current epoch (0–3) for a given block height."""
    if block_height <= GENESIS_HEIGHT:
        return 0
    raw = (block_height - GENESIS_HEIGHT) // EPOCH_LENGTH
    return min(raw, 3)


def get_reward(epoch: int) -> int:
    """Halve reward per epoch: BASE_REWARD >> epoch."""
    return BASE_REWARD >> epoch


def _format_reward(mojos: int) -> str:
    """Convert mojos to whole units (÷1000) and format as a float string."""
    value = mojos / 1000
    # Show as integer-style float if whole, otherwise up to 3 decimal places
    if value == int(value):
        return f"{int(value):,}.0"
    return f"{value:,.3f}".rstrip("0")


async def fetch_stats(current_block_height: int) -> dict:
    """Return summary statistics for the stats bar."""
    epoch = get_epoch(current_block_height)
    reward = get_reward(epoch)
    total_mined = await get_total_mined()
    return {
        "total_mined": _format_number(total_mined),
        "current_block_height": _format_number(current_block_height),
        "reward_per_block": _format_reward(reward),
        "current_epoch": epoch,
    }


def _format_time_ago(spent_height: int, current_height: int) -> str:
    """Approximate time ago from block height difference (~18.75s per block)."""
    blocks_ago = current_height - spent_height
    if blocks_ago < 0:
        return "just now"
    seconds = blocks_ago * 18.75
    if seconds < 60:
        return "just now"
    minutes = seconds / 60
    if minutes < 60:
        return f"{int(minutes)} min ago"
    hours = minutes / 60
    if hours < 24:
        if hours < 2:
            return f"{hours:.1f} hrs ago"
        return f"{int(hours)} hrs ago"
    days = hours / 24
    return f"{int(days)} days ago"


def format_recent_wins(cache: dict, current_height: int, count: int = 20) -> list[dict]:
    """
    Build formatted recent wins from the cache for the template.

    Each entry has: block_height, pubkey, reward, time_ago
    """
    raw = build_recent_wins(cache, count)
    formatted = []
    for entry in raw:
        reward_mojos = entry["reward"]
        formatted.append({
            "block_height": _format_number(entry["spent_height"]),
            "pubkey": entry["pubkey"],
            "reward": _format_number(reward_mojos / 1000),
            "time_ago": _format_time_ago(entry["spent_height"], current_height),
        })
    return formatted


async def fetch_dashboard_data(current_height: int) -> tuple[list[dict], list[dict]]:
    """
    Refresh the mine cache once and return both recent wins and leaderboard.

    Returns (recent_wins, leaderboard) formatted for the template.
    """
    cache = await refresh_cache(client)

    # Recent wins
    recent_wins = format_recent_wins(cache, current_height, count=20)

    # Leaderboard
    raw_lb = build_leaderboard(cache, count=50)
    leaderboard = []
    for entry in raw_lb:
        total_mojos = entry["total_mined"]
        leaderboard.append({
            "pubkey": entry["pubkey"],
            "total_mined": _format_number(total_mojos / 1000),
            "blocks_won": entry["blocks_won"],
        })

    return recent_wins, leaderboard


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

async def render_dashboard(template_dir: Path, output_path: Path) -> None:
    """Fetch all data, render the template, and write the output file."""
    env = Environment(
        loader=FileSystemLoader(str(template_dir)),
        autoescape=False,
    )
    template = env.get_template("template.html")

    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")

    current_height = await get_current_block_height()
    stats = await fetch_stats(current_height)
    recent_wins, leaderboard = await fetch_dashboard_data(current_height)

    context = {
        "updated_at": now,
        "stats": stats,
        "recent_wins": recent_wins,
        "leaderboard": leaderboard,
    }

    html = template.render(**context)
    output_path.write_text(html, encoding="utf-8")
    print(f"[{now}] Published dashboard → {output_path}")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

async def main() -> None:
    parser = argparse.ArgumentParser(description="XKV8 Dashboard Publisher")
    parser.add_argument(
        "--watch",
        action="store_true",
        help="Continuously re-publish on an interval",
    )
    parser.add_argument(
        "--interval",
        type=int,
        default=60,
        help="Seconds between publishes when using --watch (default: 60)",
    )
    parser.add_argument(
        "--output",
        type=str,
        default=None,
        help="Output HTML file path (default: index.html in the template directory)",
    )
    args = parser.parse_args()

    template_dir = Path(__file__).resolve().parent
    output_path = Path(args.output) if args.output else template_dir / "index.html"

    # Graceful shutdown on Ctrl+C
    def _handle_signal(sig, frame):
        print("\nStopping publisher.")
        sys.exit(0)

    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)

    if args.watch:
        print(f"Publishing every {args.interval}s. Press Ctrl+C to stop.")
        while True:
            await render_dashboard(template_dir, output_path)
            await asyncio.sleep(args.interval)
    else:
        await render_dashboard(template_dir, output_path)


if __name__ == "__main__":
    asyncio.run(main())
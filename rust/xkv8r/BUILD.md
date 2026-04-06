# XKV8 Rust Miner — Build Instructions

## Prerequisites

1. **Rust toolchain** (1.75+ recommended):

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source "$HOME/.cargo/env"
   ```

2. **System dependencies** (for `native-tls` / OpenSSL):

   - **macOS**: Already included with Xcode Command Line Tools
     ```bash
     xcode-select --install  # if not already installed
     ```
   - **Linux (Debian/Ubuntu)**:
     ```bash
     sudo apt-get install -y pkg-config libssl-dev build-essential
     ```
   - **Linux (Fedora/RHEL)**:
     ```bash
     sudo dnf install -y openssl-devel pkg-config gcc
     ```

## Building

From the repository root:

```bash
cd rust/xkv8r

# Debug build (fast compile, slower runtime)
cargo build

# Release build (optimized, LTO enabled — recommended for mining)
cargo build --release
```

The binary is produced at:
- Debug: `rust/xkv8r/target/debug/xkv8r`
- Release: `rust/xkv8r/target/release/xkv8r`

## Running

The Rust miner uses the same environment variables as the Python miner:

```bash
# Required
export MINER_SECRET_KEY="YOUR_CHOSEN_BLS_KEY"
export TARGET_ADDRESS="YOUR_XCH_REWARD_ADDRESS"

# Run (from repo root)
./rust/xkv8r/target/release/xkv8r
```

### All Environment Variables

| Variable | Default | Description |
|---|---|---|
| `TARGET_ADDRESS` | *(required)* | Bech32m address to receive mining rewards |
| `MINER_SECRET_KEY` | *(random)* | 32-byte hex seed for BLS secret key (leaderboard identity) |
| `THREAD_COUNT` | `1` | Number of threads for nonce grinding |
| `FEE_MOJOS` | `0` | Fee in mojos per spend bundle. Send XCH to the fee address printed at startup |
| `LOCAL_FULL_NODE` | *(unset)* | Enables instant-react mining via Peer subscriptions. Value: `host:port` or truthy flag |
| `PEER_PORT` | `8444` / `58444` | Chia peer protocol port (only with `LOCAL_FULL_NODE`) |
| `CHIA_ROOT` | `~/.chia/mainnet` | Path to Chia data directory (for TLS certs) |
| `TESTNET` | *(unset)* | Set to any value to target testnet11 |
| `DEFAULT_SLEEP` | `5` | Seconds between polling cycles |
| `DEBUG` | `0` | Set to `1` for verbose spend bundle JSON output |

### Example: Full-featured launch

```bash
MINER_SECRET_KEY="deadbeef..." \
TARGET_ADDRESS="xch1..." \
THREAD_COUNT="4" \
FEE_MOJOS="250000" \
LOCAL_FULL_NODE="localhost:8555" \
PEER_PORT="8444" \
./rust/xkv8r/target/release/xkv8r
```

### Running in the background

```bash
nohup ./rust/xkv8r/target/release/xkv8r >> ~/xkv8r-rust.log 2>&1 &
tail -f ~/xkv8r-rust.log
```

## Architecture

The Rust miner is a faithful port of `python/xkv8/xkv8r.py` using the native
`chia-wallet-sdk` Rust crate (not the Python bindings). Key modules:

| Module | Purpose |
|---|---|
| `main.rs` | Entry point, banner, signal handling |
| `config.rs` | Environment variable parsing, constants |
| `puzzle.rs` | Puzzle compilation, currying, epoch/reward math |
| `pow.rs` | Multi-threaded SHA-256 nonce grinding |
| `bundle.rs` | CAT spend bundle construction + optional fee |
| `client.rs` | RPC client abstraction (Coinset API + local full node) |
| `mining.rs` | Polling loop + instant-react Peer subscription loop |

Both mining modes from the Python miner are implemented:
- **Polling mode** (default): Periodically queries the Coinset API
- **Instant-react mode** (`LOCAL_FULL_NODE`): Peer protocol subscriptions with precomputed bundles

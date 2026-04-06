# Introduction

XKV8 is the first mineable Proof of Uptime and Luck token on the Chia blockchain. XKV8 mining genesis height 8,521,888.

No pre-mine. 21M issuance. 3 8-month halvings before emissions stabilize until gone. Friendly competition with a side of fun tokenomics.

Your miner will solve a small Proof of Work challenge and submit a "lode" spend to the Chia mempool. Most of the time, the first one that makes it to the mempool of the next winning Chia farmer will be rewarded XKV8 tokens. The lode coin will distribute your rewards and re-create itself to be ready for the next Chia transaction block.

# Prerequisites

You will need 

  * A BLS key `MINER_SECRET_KEY` that you keep track of. It could be an existing farmer key or wallet secret key. It will be your identity for the leaderboards and your way to register a leaderboard nickname.
  * A desired `TARGET_ADDRESS` for rewards. This is left distinct from the miner key so that you can donate rewards, burn rewards, or do whatever else you choose to do with them while maintaining your leaderboard position.

# Installation

These instructions should be accurate for Linux or Mac. Documentation submissions from interested Windows users are much appreciated!

```
git clone https://github.com/trgarrett/xkv8.git

cd xkv8

python3 -m venv venv

source venv/bin/activate

# Install Rust for chia_wallet_sdk
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

pip install -r requirements.txt

# to run in foreground
cd python

MINER_SECRET_KEY="YOUR_CHOSEN_BLS_KEY" TARGET_ADDRESS="YOUR_XCH_REWARD_ADDRESS" python3 -m xkv8.xkv8r
```

# Optional Environment Variables

In addition to the required `MINER_SECRET_KEY` and `TARGET_ADDRESS` variables, the miner supports the following optional settings:

| Variable | Default | Description |
|---|---|---|
| `THREAD_COUNT` | `1` | Number of threads used for nonce grinding. Increase this to utilise multiple CPU cores and find valid nonces faster. |
| `FEE_MOJOS` | `0` | Fee in mojos to attach to each mining spend bundle. When greater than 0, the miner looks for spendable XCH coins at the standard transaction address derived from your `MINER_SECRET_KEY` and attaches a fee spend. The fee address is printed at startup — send XCH there to fund fee-boosted mining. If no coins are available, the miner logs a warning and submits without a fee. |
| `LOCAL_FULL_NODE` | *(unset)* | When set, the miner connects to a local Chia full node via TLS-authenticated RPC as the primary client **and** enables instant-react mining via Peer protocol subscriptions (see below). If the value contains a colon it is treated as `host:port` (e.g. `localhost:8555`); otherwise the default `https://localhost:8555` is used. TLS certificates are read from `$CHIA_ROOT/config/ssl/full_node/`. |
| `PEER_PORT` | `8444` (`58444` for testnet) | Chia peer protocol port for the instant-react Peer subscription connection. Only used when `LOCAL_FULL_NODE` is set. |
| `CHIA_ROOT` | `~/.chia/mainnet` | Path to your Chia data directory. Only relevant when `LOCAL_FULL_NODE` is set, as the full-node TLS certs are loaded from here. |
| `DEBUG` | `0` | Set to `1` to log the full JSON representation of every spend bundle (including fee spends) before it is pushed to the network. Useful for troubleshooting submission issues. |

Example using all options:

```
MINER_SECRET_KEY="YOUR_CHOSEN_BLS_KEY" \
TARGET_ADDRESS="YOUR_XCH_REWARD_ADDRESS" \
THREAD_COUNT="4" \
FEE_MOJOS="250000" \
LOCAL_FULL_NODE="localhost:8555" \
PEER_PORT="8444" \
DEBUG="1" \
python3 -m xkv8.xkv8r
```

# Instant-React Mining (Advanced)

When `LOCAL_FULL_NODE` is set, the miner automatically activates **instant-react mode**. Instead of polling for new blocks every few seconds, it:

1. **Subscribes** to the lode coin puzzle hash via the Chia Peer protocol (port 8444) for real-time block notifications
2. **Precomputes** the next nonce and pre-signs a spend bundle for the predicted child coin at height+1
3. **Fires immediately** when a block confirms — pushing the prebuilt bundle to the mempool within milliseconds of the new lode coin appearing on-chain
4. **Caches** CAT lineage proofs (derived via `Cat.child()` with zero RPC calls) and fee coins (refreshed only on state change events)

If the Peer connection fails, the miner automatically falls back to the standard polling loop.

Without `LOCAL_FULL_NODE`, the miner uses the standard polling loop against the public Coinset API — no changes to that path.

# FAQ

1. Q: Why isn't there a release version?

    A: I'd like to encourage everyone to read and understand the source code. A release will likely come later after everyone is comfortable with the concept.

    Furthermore, this is supposed to be fun! I know of AT LEAST 3 ways you could modify this miner to make yourself more competitive. You could open source your changes OR keep them all to yourself for an extra rewards boost.

    There is no such thing as cheating. This is a strictly "code is law" situation. May the most creative miner win!


2. Q: What does this do?

   A: It provides XKV8 CAT rewards to any and all participants in a well-defined way with an emissions schedule.


3. Q: Is your CAT special somehow?

   A: No, it is a standard single-mint CAT. All special logic lives in the "lode" puzzle that spends out the emissions. See `clsp/puzzle.clsp`.

4. Q: Can I increase my odds?
   
   A: First, keep your miner up and running. If block fee pressure becomes a thing, you can attach a fee by setting `FEE_MOJOS` (e.g. `FEE_MOJOS="250000"`). The miner will print a fee address at startup — send some XCH there and the miner will automatically include a fee coin in each spend bundle. Because of Chia's mempool Replace By Fee (RBF) rules, the first fee-paying spend that makes it to the Chia winning farmer will likely be chosen. You can try to buy your luck, but it will only go so far. But Chia farmers would love for you to try anyway!

   Whether or not you can mine a winning solution in time is greatly dependent on block propagation, block intervals, mempool propagation, custom mempool implementations, and various other minutiae. Good luck!

5. Q: How can I make the miner run in the background?

   A: nohup or your preferred daemon platform should work fine

   Here's an example bash script to background it
   ```
    #!/bin/bash
    export MINER_SECRET_KEY="<MINER_SECRET_KEY>"
    export TARGET_ADDRESS="<TARGET_ADDRESS>"
    cd ~/xkv8/
    source venv/bin/activate
    cd python
    nohup python3 -u -m xkv8.xkv8r >> ~/xkv8/xkv8r.log 2>&1 &
    echo Launched
   ```

   Then, to watch the logs, a simple `tail -f ~/xkv8/xkv8r.log`
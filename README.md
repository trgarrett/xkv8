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
   
   A: First, keep your miner up and running. If block fee pressure becomes a thing, you could choose to inject a fee (some assembly required). Because of Chia's mempool Replace By Fee (RBF) rules, the first fee-paying spend that makes it to the Chia winning farmer will likely be chosen. You can try to buy your luck, but it will only go so far. But Chia farmers would love for you to try anyway!

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
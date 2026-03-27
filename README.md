# Prerequisites
These instructions should be accurate for Linux or Mac.

You will need 

  * A BLS key `MINER_SECRET_KEY` that you keep track of. It could be an existing farmer key or wallet secret key. It will be your identity for the leaderboards and your way to register a leaderboard nickname.
  * A desired `TARGET_ADDRESS` for rewards. This is left distinct from the miner key so that you can donate rewards, burn rewards, or do whatever else you choose to do with them while maintaining your leaderboard position.

# Installation

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


2. Q: What does this do?

   A: It provides XKV8 CAT rewards to any and all participants in a well-defined way with an emissions schedule.


3. Q: Is your CAT special somehow?

   A: No, it is a standard single-mint CAT. All special logic lives in the "lode" puzzle that spends out the emissions. See `clsp/puzzle.clsp`.
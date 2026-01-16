import asyncio
import os
import signal
import sys
import json
from collections import deque

from chia_wallet_sdk import Address, CatSpend, Clvm, CoinsetClient, Signature, Spend, SpendBundle, cat_puzzle_hash
from xkv8.utils import spend_bundle_to_json

# global settings, suitable for deriving a new mineable CAT
LODE_PUZZLE_BYTES = bytes.fromhex("ff02ffff01ff04ffff04ff0affff04ff05ff808080ffff04ffff04ff04ffff01ff018080ffff04ffff04ff0effff04ff17ffff04ff05ffff04ffff04ff17ff8080ff8080808080ff80808080ffff04ffff01ff52ff4933ff018080")
LODE_PUZZLEHASH = Clvm().deserialize(LODE_PUZZLE_BYTES).puzzle().puzzle_hash
TAIL_HASH = bytes.fromhex("4eadfa450c19fa51df65eb7fbf5b61077ec80ec799a7652bb187b705bff19a90")

# environment specific
TARGET_ADDRESS = os.environ.get("TARGET_ADDRESS", "txch1s8e2m4veaymm6n5v0k22yg89urud79np9knnx8dux80mgqmfw30qp3gy3e")
TARGET_PUZZLEHASH = Address.decode(TARGET_ADDRESS).puzzle_hash

recent_coins: deque = deque([], 50) # bytes of coinIDs already attempted

async def mine():
    client = CoinsetClient.testnet11()
    last_height = -1
    print("")
    while True:
        blockchain_state = await client.get_blockchain_state()
        if blockchain_state.success:
            height = blockchain_state.blockchain_state.peak.height
            if height != last_height:
                last_height = height
                if height % 100 == 0:
                    print(f"Mining at height: {height}")
        else:
            print("Failed to get blockchain state")
            await asyncio.sleep(5)
            continue

        unspent_crs = await client.get_coin_records_by_hint(LODE_PUZZLEHASH, start_height=None, end_height=None, include_spent_coins=False)
        if unspent_crs.success:
            for cr in unspent_crs.coin_records:
                parent_res = await client.get_coin_record_by_name(cr.coin.parent_coin_info)
                if parent_res.success and parent_res.coin_record is not None:
                    parent = parent_res.coin_record
                    gps_res = await client.get_puzzle_and_solution(parent.coin.coin_id(), parent.spent_block_index)
                    if gps_res.success:
                        clvm = Clvm()
                        puzzle = clvm.deserialize(gps_res.coin_solution.puzzle_reveal).puzzle()
                        parent_solution = clvm.deserialize(gps_res.coin_solution.solution)
                        cats = puzzle.parse_child_cats(parent_coin=parent.coin, parent_solution=parent_solution)
                        for cat in cats:
                            if cat.info.p2_puzzle_hash == LODE_PUZZLEHASH and cat.info.asset_id == TAIL_HASH:
                                cat_lode_puzzlehash = cat_puzzle_hash(asset_id=TAIL_HASH, inner_puzzle_hash=LODE_PUZZLEHASH)
                                if cr.coin.coin_id() in recent_coins:
                                    continue
                                print("Found lode coin: ", cr.coin.coin_id().hex())
                                recent_coins.append(cr.coin.coin_id())
                                assert cr.coin.puzzle_hash == cat_lode_puzzlehash
                                lode_puzzle = clvm.deserialize(LODE_PUZZLE_BYTES)
                                solution_string = f"({cr.coin.amount} {1 + last_height} 0x{TARGET_PUZZLEHASH.hex()})"
                                spend = Spend(lode_puzzle, clvm.parse(solution_string))
                                cat_spend = CatSpend(cat, spend)
                                clvm.spend_cats([cat_spend])
                                bundle = SpendBundle(coin_spends=clvm.coin_spends(), aggregated_signature=Signature.infinity())
                                try:
                                    tx_result = await client.push_tx(bundle)
                                except Exception as e:
                                    print("Failed to push tx: ", repr(e))
                                    try:
                                        pass
                                        #print(spend_bundle_to_json(bundle))
                                    except Exception:
                                        # fallback to raw bytes if JSON helper fails
                                        try:
                                            print(bundle.to_bytes().hex())
                                        except Exception:
                                            print(repr(bundle))
                                    continue
                                if tx_result.success:
                                    print("Submitted spend bundle, status:", tx_result.status, " Now it's just luck!")
        else:
            print("Failed to discover unspent coins")
            continue
            
        await asyncio.sleep(10)

def main():
    banner = """
 __   ___  __      _____  
 \ \ / / | \ \    / / _ \ 
  \ V /| | _\ \  / / (_) |
   > < | |/ /\ \/ / > _ < 
  / . \|   <  \  / | (_) |
 /_/ \_\_|\_\  \/   \___/ 
"""
    print(banner)
    print("Starting miner...mining to address: ", TARGET_ADDRESS)
    #print("Searching for rewards to claim from puzzlehash: ", LODE_PUZZLEHASH.hex())
    signal.signal(signal.SIGINT, sigint_handler)
    asyncio.run(mine())

def sigint_handler(_, __):
    print("\nGoodbye!")
    sys.exit(0)

if __name__ == "__main__":
    main()
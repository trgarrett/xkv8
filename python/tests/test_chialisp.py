import asyncio
import pytest
from chia_wallet_sdk import Clvm, Program

@pytest.mark.asyncio
async def test_puzzle():
    clvm = Clvm()
    with open("clsp/puzzle.clsp.hex", "r") as f:
        puzzle_source = f.read()
        puzzle = clvm.deserialize(bytes.fromhex(puzzle_source))
        solution = clvm.deserialize(bytes.fromhex("ff01ff02ff03ff04ff0580")) # (1 2 3 4 5)
        output = puzzle.run(solution, max_cost=11_000_000_000, mempool_mode=True)
        assert output is not None
        
        # Convert Program to array of results
        assert 4 == len(output.value.to_list())

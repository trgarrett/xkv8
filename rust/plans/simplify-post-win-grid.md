# Simplify Post-Win/Post-Spend Grid Management

## Problem Summary

After a win (or any lode coin spend), the precomputed bundle grid becomes stale.
The current code has complex speculative logic (`current_cat_confirmed`, speculative
child advance) that creates multiple failure modes:

1. **UNKNOWN_UNSPENT spam**: `NewPeakWallet` fires bundles for a child coin that
   doesn't exist on-chain yet.
2. **Stale grid entries**: Grid entries reference the old (spent) coin for several
   blocks until `CoinStateUpdate` arrives.
3. **Fee coin staleness**: Fee coins consumed in the winning spend aren't refreshed.
4. **Grid misses**: The grid doesn't match the new coin when `CoinStateUpdate`
   finally arrives, forcing a slow fresh build.

## Key Insight

The child coin is **deterministic** regardless of who mines the parent:
- `parent_coin_info = parent.coin_id()`
- `puzzle_hash = full_cat_ph` (constant)
- `amount = parent.amount - 10000` (reward is constant in epoch 0)

This means the grid's gen=1+ entries correctly predict the child coin for ANY
miner's spend, not just ours.

## Simplified Design

### Remove `current_cat_confirmed`

This flag is unnecessary. Instead, use a simpler rule:

### `NewPeakWallet` fire rule

Only fire from `NewPeakWallet` if `current_cat`'s coin is **known to be unspent**.
Detect this by:
- If `current_cat.coin_id()` is in `submitted_coins` → parent spend is pending,
  don't fire (we already submitted).
- If `current_cat.coin_id()` was just speculatively advanced → the coin doesn't
  exist on-chain yet. Track this with a simple `pending_parent_spend: Option<Bytes32>`
  that records the parent coin_id we're waiting to see spent.

Actually, even simpler:

### Don't speculatively advance `current_cat` at all

After a win or any spend detection:
1. **Don't change `current_cat`** — leave it pointing at the spent coin.
2. The spent coin is in `submitted_coins`, so `NewPeakWallet` won't fire for it
   (`already_submitted = true`).
3. When `CoinStateUpdate` arrives with the child coin (created, not spent), the
   handler:
   a. Looks up the grid — gen=1 entries match the child ✓
   b. Fires the precomputed bundle immediately
   c. Sets `current_cat` to the confirmed child
   d. Rebuilds the grid

This eliminates:
- The `current_cat_confirmed` flag entirely
- The speculative advance logic in both `CoinStateUpdate` and `NewPeakWallet` handlers
- The fee coin refresh after win (handled naturally when grid rebuilds after
  `CoinStateUpdate`)

### But what about grid expiry?

Grid entries expire when `target_height + 2 < new_height`. If the `CoinStateUpdate`
for the child takes 5+ blocks to arrive, the gen=1 entries expire.

**Solution**: When the grid is fully expired and `current_cat` is in `submitted_coins`
(meaning we're waiting for the child), rebuild the grid rooted at `current_cat`
(the spent parent). The gen=1+ entries will cover the child coin at the new heights.

This already happens in the `NewPeakWallet` handler:
```rust
if bundle_grid.is_empty() {
    // recompute grid
}
```

### What about the `CoinStateUpdate` spent handler?

Currently it has two branches:
- `we_submitted = true` → `check_mining_results` → speculative advance
- `we_submitted = false` → rival spend → re-bootstrap from RPC

**Simplified**:
- `we_submitted = true` → `check_mining_results` (for win/loss logging only).
  Don't advance `current_cat`. The child will arrive via `CoinStateUpdate`.
- `we_submitted = false` → same as above. Don't re-bootstrap. The child will
  arrive via `CoinStateUpdate`.

In both cases, the grid already has gen=1+ entries for the child.

## Summary of Changes

1. **Remove** `current_cat_confirmed` flag and all assignments
2. **Remove** speculative `current_cat` advance from both win-detection paths
3. **Remove** fee coin refresh from win-detection paths (moved to grid rebuild)
4. **Keep** `check_mining_results` returning `Vec<Bytes32>` (for win/loss logging)
5. **Keep** the `CoinStateUpdate` new-coin handler as the primary path for
   advancing `current_cat` and firing precomputed bundles
6. **Ensure** grid rebuild after `CoinStateUpdate` new-coin always refreshes fee coins
7. **Ensure** `NewPeakWallet` grid-empty rebuild works correctly when `current_cat`
   is a spent coin (gen=1+ entries cover the child)

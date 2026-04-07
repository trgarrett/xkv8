# Fix: Polling fallback PENDING retry → transport error cascade

## Diagnosis

The log shows a repeating cycle with **two distinct events per poll tick**:

```
polling: PENDING (unconfirmed) — retrying in 0.1s (attempt 1/15)
Failed to push tx (transport error): Some("error decoding response body")
  client[0]: http_err: error decoding response body
```

### Step-by-step trace

1. `poll_once` calls `push_tx_with_retry` → first `push_tx_to_all` → coinset.io returns `success=true, status="PENDING"`
2. `push_tx_with_retry` sees `is_pending=true`, logs the "retrying" message, sleeps 100ms, and calls `push_tx_to_all` **again**
3. The second call to coinset.io returns something that the SDK can't deserialize (`"error decoding response body"`)
4. `push_tx_with_retry` sees `is_pending=false, is_not_ready=false` → breaks loop, returns the **transport failure** result
5. `poll_once` sees `result.success=false` → **never inserts coin into `submitted_coins`**
6. On the next poll tick the coin is still unspent, not in `submitted_coins` → entire cycle repeats

The tx IS reaching the mempool (the PENDING response proves this). The problem is that the retry clobbers the initial success with a transport error, making the caller think the push failed and preventing the coin from being tracked as submitted.

### Why does the second push fail?

Unknown without raw body logging — most likely candidates:
- coinset.io rate-limits or rejects rapid duplicate pushes with a non-JSON response (HTML error page, `429 Too Many Requests`, etc.)
- The node returns a different JSON schema when the tx is already in mempool that the SDK's deserializer can't handle

---

## Fixes

### 1. `push_tx_with_retry` — preserve best-known-good result across retries

**File:** [`xkv8r/src/client.rs`](xkv8r/src/client.rs) → actually in [`xkv8r/src/mining.rs`](xkv8r/src/mining.rs:216)

**Current behavior:** `result` is unconditionally overwritten by every retry call, including ones that return a transport error.

**Fix:** Track a `best_result` alongside `result`. Whenever a call returns `success=true`, store it as `best_result`. After the loop (or on break), return `best_result` if it exists, otherwise `result`.

```rust
async fn push_tx_with_retry(...) -> PushTxResult {
    let mut result = push_tx_to_all(clients, bundle).await;
    let mut best_result: Option<PushTxResult> = if result.success {
        Some(/* clone/capture */ ...)
    } else {
        None
    };

    for attempt in 1..=COIN_NOT_READY_MAX_RETRIES {
        let is_pending = result.success && ...;
        let is_not_ready = !result.success && result.error_category == "coin_not_ready";

        if !is_pending && !is_not_ready {
            break;
        }

        // log as before...
        tokio::time::sleep(...).await;
        result = push_tx_to_all(clients, bundle).await;

        if result.success {
            best_result = Some(/* capture */);
        }
    }

    // Return best success seen, fall back to last result if nothing succeeded
    best_result.unwrap_or(result)
}
```

Because `PushTxResult` is not `Clone`, the simplest approach is to re-run the success check after the loop and reconstruct as needed, or add `#[derive(Clone)]` to `PushTxResult` in `client.rs`.

### 2. Add `#[derive(Clone)]` to `PushTxResult`

**File:** [`xkv8r/src/client.rs`](xkv8r/src/client.rs:232)

This is a pre-requisite for capturing `best_result` cleanly. The struct only contains `String`, `Option<String>`, `Vec<(usize, String)>`, and `&'static str` — all `Clone`.

### 3. Logging in `push_tx_with_retry` — log per-client errors on mid-retry transport failure

When a retry attempt itself returns a transport error, log the per-client breakdown **at that point**, so we can see what coinset.io actually returned. Currently this info is lost because the transport result replaces the previous one and the outer caller only logs the final state.

```rust
// Inside the retry loop, after `result = push_tx_to_all(...)`:
if !result.success && result.error_category == "transport" {
    eprintln!(
        "[retry {attempt}] transport error during retry for {label}: {:?}",
        result.error
    );
    for (i, summary) in &result.per_client_errors {
        eprintln!("  client[{i}]: {summary}");
    }
    // Do NOT continue retrying after a transport error — break and
    // let the best_result logic handle it.
    break;
}
```

### 4. Logging in `poll_once` transport error path — add coin_id and height

**File:** [`xkv8r/src/mining.rs`](xkv8r/src/mining.rs:479)

Currently the error just prints a generic transport message. Adding `coin_id` and `mine_height` makes it much easier to correlate across retries:

```rust
"transport" => {
    eprintln!(
        "Failed to push tx (transport error) for coin {} at height {mine_height}: {:?}",
        hex::encode(coin_id_key),
        result.error
    );
    for (i, summary) in &result.per_client_errors {
        eprintln!("  client[{i}]: {summary}");
    }
}
```

### 5. (Diagnostic) Log the raw response body on `error decoding response body`

This is the most valuable thing for understanding *why* the second push fails. The error comes from within `chia_wallet_sdk`'s HTTP layer — we can't easily intercept it there. However, we can add a `DEBUG` log in `push_tx_with_retry` to note the raw error string whenever we see the keyword `"error decoding response body"`:

```rust
if result.error.as_deref().map_or(false, |e| e.contains("error decoding response body")) {
    eprintln!("[debug] Raw decode error for {label}: {:?}", result.error);
    // Possibly also log per_client_errors here
}
```

---

## Summary of changes

| File | Change |
|------|--------|
| [`mining.rs`](xkv8r/src/mining.rs) | `push_tx_with_retry`: add `best_result` tracking; return first success even if a later retry fails |
| [`mining.rs`](xkv8r/src/mining.rs) | `push_tx_with_retry`: break on mid-retry transport error (don't keep retrying) and log per-client errors immediately |
| [`mining.rs`](xkv8r/src/mining.rs) | `poll_once` transport error path: add `coin_id` and `mine_height` to error log |
| [`client.rs`](xkv8r/src/client.rs) | Add `#[derive(Clone)]` to `PushTxResult` |

---

## Expected outcome after fix

- First PENDING push records the coin in `submitted_coins` → poll loop stops retrying
- If coinset.io consistently rejects the retry push, we stop hammering it on the same attempt
- We get per-client and per-attempt logs showing exactly what coinset.io returns on the second call

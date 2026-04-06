//! Proof-of-Work helpers: nonce grinding and CLVM-style integer encoding.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chia_protocol::Bytes32;
use num_bigint::BigUint;
use sha2::{Digest, Sha256};

/// Encode an integer as CLVM-style signed big-endian bytes.
///
/// CLVM atoms represent integers in two's-complement big-endian form with
/// minimal length.  Zero encodes as an empty byte string.
pub fn int_to_clvm_bytes(n: i64) -> Vec<u8> {
    if n == 0 {
        return Vec::new();
    }
    // (n.bit_length() + 8) // 8 => enough room for sign bit
    let byte_len = ((64 - (n.abs() as u64).leading_zeros() as usize) + 8) / 8;
    n.to_be_bytes()[8 - byte_len..].to_vec()
}

/// Encode a u32 as CLVM-style signed big-endian bytes.
pub fn int_to_clvm_bytes_u32(n: u32) -> Vec<u8> {
    int_to_clvm_bytes(i64::from(n))
}

/// SHA-256 of concatenated byte slices.
pub fn pow_sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Search a nonce range `[start, end)` for a valid PoW.
///
/// Returns `Some(nonce)` on success.  Stops early if `found` or `cancel` is set.
fn search_nonce_range(
    inner_puzzle_hash: &[u8; 32],
    miner_pubkey_bytes: &[u8],
    h_bytes: &[u8],
    difficulty: &BigUint,
    start: u64,
    end: u64,
    found: &AtomicBool,
    cancel: &AtomicBool,
) -> Option<u64> {
    for nonce in start..end {
        if found.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
            return None;
        }
        let n_bytes = int_to_clvm_bytes(nonce as i64);
        let mut hasher = Sha256::new();
        hasher.update(inner_puzzle_hash);
        hasher.update(miner_pubkey_bytes);
        hasher.update(h_bytes);
        hasher.update(&n_bytes);
        let digest: [u8; 32] = hasher.finalize().into();

        let pow_int = BigUint::from_bytes_be(&digest);
        if !pow_int.eq(&BigUint::ZERO) && *difficulty > pow_int {
            found.store(true, Ordering::Relaxed);
            return Some(nonce);
        }
    }
    None
}

/// Grind for a nonce that satisfies the PoW target.
///
/// Uses up to `thread_count` threads.  Returns `None` if no valid nonce
/// is found within `max_attempts`, or if `cancel` is signalled.
pub fn find_valid_nonce(
    inner_puzzle_hash: &Bytes32,
    miner_pubkey_bytes: &[u8],
    user_height: u32,
    difficulty_bits: u32,
    max_attempts: u64,
    thread_count: usize,
    cancel: Arc<AtomicBool>,
) -> Option<u64> {
    let h_bytes = int_to_clvm_bytes_u32(user_height);
    let difficulty = BigUint::from(1u32) << difficulty_bits;
    let iph: [u8; 32] = (*inner_puzzle_hash).into();

    if thread_count <= 1 {
        // Single-threaded fast path
        return search_nonce_range(
            &iph,
            miner_pubkey_bytes,
            &h_bytes,
            &difficulty,
            0,
            max_attempts,
            &AtomicBool::new(false),
            &cancel,
        );
    }

    // Multi-threaded path
    let found = Arc::new(AtomicBool::new(false));
    let chunk_size = (max_attempts + thread_count as u64 - 1) / thread_count as u64;

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for i in 0..thread_count {
            let start = i as u64 * chunk_size;
            let end = (start + chunk_size).min(max_attempts);
            if start >= max_attempts {
                break;
            }
            let found = Arc::clone(&found);
            let cancel = Arc::clone(&cancel);
            let h_bytes = h_bytes.clone();
            let difficulty = difficulty.clone();
            handles.push(s.spawn(move || {
                search_nonce_range(
                    &iph,
                    miner_pubkey_bytes,
                    &h_bytes,
                    &difficulty,
                    start,
                    end,
                    &found,
                    &cancel,
                )
            }));
        }
        for handle in handles {
            if let Some(nonce) = handle.join().unwrap_or(None) {
                return Some(nonce);
            }
        }
        None
    })
}

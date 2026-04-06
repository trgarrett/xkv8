//! Environment-based configuration for the XKV8 miner.

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chia_bls::SecretKey;
use chia_protocol::Bytes32;
use chia_wallet_sdk::utils::Address;
use hex_literal::hex;

// ── Puzzle parameters ──────────────────────────────────────────────────
pub const CAT_TAIL_HASH: Bytes32 = Bytes32::new(hex!(
    "f09c8d630a0a64eb4633c0933e0ca131e646cebb384cfc4f6718bad80859b5e8"
));

pub const GENESIS_HEIGHT: u32 = 8_521_888;
pub const EPOCH_LENGTH: u32 = 1_120_000;
pub const BASE_REWARD: u64 = 10_000; // mojos
pub const BASE_DIFFICULTY_BITS: u32 = 238; // 2^238

// ── Genesis challenges ─────────────────────────────────────────────────
pub const MAINNET_GENESIS_CHALLENGE: Bytes32 = Bytes32::new(hex!(
    "ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb"
));

pub const TESTNET11_GENESIS_CHALLENGE: Bytes32 = Bytes32::new(hex!(
    "37a90eb5185a9c4439a91ddc98bbadce7b4feba060d50116a067de66bf236615"
));

/// Fully-resolved miner configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub target_address: String,
    pub target_puzzlehash: Bytes32,
    pub miner_sk: SecretKey,
    pub thread_count: usize,
    pub fee_mojos: u64,
    pub local_full_node: Option<String>,
    pub network_name: String,
    pub is_testnet: bool,
    pub chia_root: PathBuf,
    pub peer_port: u16,
    pub default_sleep_secs: f64,
    pub debug: bool,
    pub genesis_challenge: Bytes32,
}

impl Config {
    /// Build configuration from environment variables.
    pub fn from_env() -> Result<Self> {
        // TARGET_ADDRESS (required)
        let target_address = env::var("TARGET_ADDRESS")
            .context("Required TARGET_ADDRESS environment variable not set")?;

        // TESTNET detection
        let is_testnet = env::var("TESTNET").is_ok();
        let network_name = if is_testnet {
            "testnet11".to_string()
        } else {
            "mainnet".to_string()
        };

        // Validate address prefix
        if !is_testnet && !target_address.starts_with("xch") {
            bail!("TARGET_ADDRESS must be a mainnet address (starting with 'xch')");
        } else if is_testnet && !target_address.starts_with("txch") {
            bail!("TARGET_ADDRESS must be a testnet address (starting with 'txch')");
        }

        // Decode target puzzle hash
        let decoded = Address::decode(&target_address)
            .context("Failed to decode TARGET_ADDRESS")?;
        let target_puzzlehash = decoded.puzzle_hash;

        // MINER_SECRET_KEY
        let miner_key_hex = env::var("MINER_SECRET_KEY").unwrap_or_default();
        let miner_sk = if !miner_key_hex.is_empty() {
            let seed = hex::decode(&miner_key_hex)
                .context("MINER_SECRET_KEY must be valid hex")?;
            SecretKey::from_seed(&seed)
        } else {
            let seed: [u8; 32] = rand::random();
            eprintln!(
                "No MINER_SECRET_KEY set – generated ephemeral key. \
                 You will be able to mine, but leaderboard standings will be impacted!"
            );
            SecretKey::from_seed(&seed)
        };

        // THREAD_COUNT
        let thread_count: usize = env::var("THREAD_COUNT")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .context("THREAD_COUNT must be a positive integer")?;

        // FEE_MOJOS
        let fee_mojos: u64 = env::var("FEE_MOJOS")
            .unwrap_or_else(|_| "0".into())
            .parse()
            .context("FEE_MOJOS must be a non-negative integer")?;

        // LOCAL_FULL_NODE
        let local_full_node = env::var("LOCAL_FULL_NODE").ok();

        // CHIA_ROOT
        let chia_root = env::var("CHIA_ROOT").map(PathBuf::from).unwrap_or_else(|_| {
            dirs_home().join(".chia").join(&network_name)
        });

        // PEER_PORT
        let default_peer_port = if is_testnet { 58444 } else { 8444 };
        let peer_port: u16 = env::var("PEER_PORT")
            .unwrap_or_else(|_| default_peer_port.to_string())
            .parse()
            .context("PEER_PORT must be a valid port number")?;

        // DEFAULT_SLEEP
        let default_sleep_secs: f64 = env::var("DEFAULT_SLEEP")
            .unwrap_or_else(|_| "5".into())
            .parse()
            .context("DEFAULT_SLEEP must be a number")?;

        // DEBUG
        let debug = env::var("DEBUG").map(|v| v == "1").unwrap_or(false);

        // Genesis challenge
        let genesis_challenge = if is_testnet {
            TESTNET11_GENESIS_CHALLENGE
        } else {
            MAINNET_GENESIS_CHALLENGE
        };

        Ok(Config {
            target_address,
            target_puzzlehash,
            miner_sk,
            thread_count,
            fee_mojos,
            local_full_node,
            network_name,
            is_testnet,
            chia_root,
            peer_port,
            default_sleep_secs,
            debug,
            genesis_challenge,
        })
    }
}

/// Return the user's home directory.
fn dirs_home() -> PathBuf {
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

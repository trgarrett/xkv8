mod bundle;
mod client;
mod config;
mod mining;
mod pow;
mod puzzle;

use std::process;
use std::sync::Arc;

use anyhow::Result;
use config::Config;
use tracing_subscriber::EnvFilter;

const BANNER: &str = r#"
__   ___  __      _____   _ __ 
 \ \ / / | \ \    / / _ \ | '__|
  \ V /| | _\ \  / / (_) || |   
   > < | |/ /\ \/ / > _ < | |   
  / . \|   <  \  / | (_) || |   
 /_/ \_\_|\_\  \/   \___/ |_|
"#;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    println!("{BANNER}");
    println!("Starting miner (Rust)...");

    let config = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Configuration error: {e}");
            process::exit(1);
        }
    };

    // Install Ctrl-C handler
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.ok();
        println!("\nGoodbye!");
        process::exit(0);
    });

    if let Err(e) = mining::mine(config).await {
        eprintln!("Fatal mining error: {e}");
        process::exit(1);
    }

    Ok(())
}

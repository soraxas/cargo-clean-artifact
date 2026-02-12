use anyhow::Result;
use clap::Parser;

use crate::cli::CliArgs;

mod clean;
mod cli;
mod crate_deps;
mod trace_parser;

#[tokio::main]
async fn main() -> Result<()> {
    // Simple env_logger for debugging (only shows on RUST_LOG=debug)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args = CliArgs::parse();
    args.run().await?;

    Ok(())
}

use anyhow::Result;

use crate::cli::CliArgs;

mod clean;
mod cli;
mod crate_deps;
mod theme;
mod trace_parser;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse args first to check verbose flag (handles cargo subcommand invocation)
    let args = CliArgs::parse_args();

    // Initialize logger based on verbose flag
    // When verbose is set, always use debug level (override RUST_LOG)
    if args.is_verbose() {
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Debug)
            .init();
    } else {
        // Otherwise respect RUST_LOG or default to warn
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    args.run().await?;

    Ok(())
}

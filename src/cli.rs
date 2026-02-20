use crate::clean::CleanCommand;
use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "cargo-clean-artifact",
    bin_name = "cargo clean-artifact",
    about = "Clean old build artifacts / deps that are not used in any features of a workspace."
)]
pub struct CliArgs {
    #[command(flatten)]
    cmd: CleanCommand,
}

impl CliArgs {
    /// Parse arguments, handling cargo subcommand invocation
    pub fn parse_args() -> Self {
        // When invoked as `cargo clean-artifact`, cargo passes "clean-artifact" as the first argument
        // We need to skip it
        let mut args: Vec<_> = std::env::args().collect();
        
        // Check if second argument is "clean-artifact" (when run as cargo subcommand)
        if args.len() > 1 && args[1] == "clean-artifact" {
            args.remove(1);
        }
        
        Self::parse_from(args)
    }

    pub async fn run(self) -> Result<()> {
        self.cmd.run().await
    }

    pub fn is_verbose(&self) -> bool {
        self.cmd.is_verbose()
    }
}

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
    pub async fn run(self) -> Result<()> {
        self.cmd.run().await
    }
}

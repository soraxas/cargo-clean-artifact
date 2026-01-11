use crate::analyse::AnalyseCommand;
use crate::clean::CleanCommand;
use crate::git::CleanGitCommand;
use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "cargo-clean-artifact",
    bin_name = "cargo clean-artifact",
    about = "Clean old build artifacts / deps that are not used in any features of a workspace."
)]
pub struct CliArgs {
    /// Available subcommands.
    #[command(subcommand)]
    cmd: InnerCmd,
}

impl CliArgs {
    pub async fn run(self) -> Result<()> {
        match self.cmd {
            InnerCmd::Clean(cmd) => {
                cmd.run().await?;
            }
            InnerCmd::CleanGit(cmd) => {
                cmd.run().await?;
            }
            InnerCmd::Analyse(cmd) => {
                cmd.run().await?;
            }
        }

        Ok(())
    }
}

#[derive(Debug, Subcommand)]
enum InnerCmd {
    /// Clean unused artifacts from cargo target directories.
    Clean(CleanCommand),
    /// Remove gone local git branches.
    CleanGit(CleanGitCommand),
    /// Analyze which target artifacts map to which packages/features.
    Analyse(AnalyseCommand),
}

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result};
use clap::{ArgAction, Args, ValueHint};
use futures::future::try_join_all;
use tokio::process::Command;

/// Remove gone local git branches.
#[derive(Debug, Args)]
pub(crate) struct CleanGitCommand {
    /// Actually delete branches (dry-run is default).
    #[clap(short = 'y', long = "yes")]
    yes: bool,

    /// Force dry-run.
    #[clap(long, action = ArgAction::SetTrue)]
    dry_run: bool,

    /// Directory to search for git repos.
    #[clap(
        value_hint = ValueHint::DirPath,
        default_value = ".",
        value_name = "DIR"
    )]
    dir: PathBuf,

    /// Run `git fetch --all` before pruning.
    #[clap(long, default_value_t = true, action = ArgAction::Set)]
    fetch: bool,
}

impl CleanGitCommand {
    fn is_dry_run(&self) -> bool {
        !self.yes || self.dry_run
    }

    pub async fn run(self) -> Result<()> {
        let git_projects = find_git_projects(&self.dir)
            .await
            .with_context(|| format!("failed to find git projects from {}", self.dir.display()))?;

        if self.fetch {
            try_join_all(
                git_projects
                    .iter()
                    .map(|git_dir| run_git_fetch_all(git_dir)),
            )
            .await
            .context("failed to run git fetch step")?;
        }

        let dry_run = self.is_dry_run();

        try_join_all(git_projects.iter().map(|git_dir| async move {
            remove_dead_branches(dry_run, git_dir)
                .await
                .context(format!(
                    "failed to clean up dead branches in {}",
                    git_dir.display()
                ))
        }))
        .await
        .context("failed to clean up dead branches")?;

        Ok(())
    }
}

async fn find_git_projects(dir: &Path) -> Result<Vec<PathBuf>> {
    /// Find recursively git projects from the given directory.
    async fn find(dir: &Path) -> Result<Vec<PathBuf>> {
        Ok(vec![dir.to_path_buf()])
    }

    // TODO: Check if `dir` is in a git repository.

    find(dir).await
}

/// - `dir`: The root directory of git repository.
async fn run_git_fetch_all(git_dir: &Path) -> Result<()> {
    let mut c = Command::new("git");
    c.arg("fetch").arg("--all");
    c.kill_on_drop(true);

    // TODO: Log status code
    let _status = c.status().await.with_context(|| {
        format!(
            "failed to get status of `git fetch --all` for {}",
            git_dir.display()
        )
    })?;

    Ok(())
}

async fn remove_dead_branches(dry_run: bool, git_dir: &Path) -> Result<()> {
    let branches = Command::new("git")
        .arg("for-each-ref")
        .arg("--format")
        .arg("%(refname:short) %(upstream:track)")
        .current_dir(git_dir)
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to get git refs")?;

    let branches =
        String::from_utf8(branches.stdout).context("failed to parse output of git refs as utf9")?;

    for line in branches.lines() {
        let items = line.split_whitespace().collect::<Vec<_>>();
        if items.len() == 2 && items[1] == "[gone]" {
            let branch = items[0];

            if dry_run {
                println!("git branch -D {} # dry-run: {}", branch, git_dir.display());
            } else {
                let _status = Command::new("git")
                    .arg("branch")
                    .arg("-D")
                    .arg(branch)
                    .current_dir(git_dir)
                    .kill_on_drop(true)
                    .status()
                    .await
                    .with_context(|| format!("failed to delete branch {}", branch,))?;
            }
        }
    }

    Ok(())
}

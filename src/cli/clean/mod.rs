use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use anstyle::{AnsiColor, Reset, Style};
use anyhow::{Context, Result};
use clap::{ArgAction, Args, ValueHint};
use futures::future::try_join_all;
use std::io::IsTerminal;
use tokio::process::Command;

use crate::util::wrap;

mod cargo;

/// Clean unused, old project files.
///
/// 1. This removes
///
///  - the unused files in `target` directory.
#[derive(Debug, Args)]
pub(crate) struct CleanCommand {
    /// Actually remove files (dry-run is the default).
    #[clap(short = 'y', long = "yes")]
    yes: bool,

    /// Force dry-run mode (default behavior).
    #[clap(long, action = ArgAction::SetTrue)]
    dry_run: bool,

    /// The directory to clean.
    ///
    #[clap(
        value_hint = ValueHint::DirPath,
        default_value = ".",
        value_name = "DIR"
    )]
    dir: PathBuf,
}

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

impl CleanCommand {
    fn is_dry_run(&self) -> bool {
        !self.yes || self.dry_run
    }

    pub async fn run(self) -> Result<()> {
        let git_projects = find_git_projects(&self.dir)
            .await
            .with_context(|| format!("failed to find git projects from {}", self.dir.display()))?;

        let remove_unused_files = async {
            let stats = try_join_all(
                git_projects
                    .iter()
                    .map(|git_dir| self.remove_unused_files_of_cargo(git_dir)),
            )
            .await
            .context("failed to clean up unused files")?;

            let total = stats
                .into_iter()
                .fold(cargo::CleanupStats::default(), |mut acc, s| {
                    acc.merge_from(s);
                    acc
                });

            Ok::<_, anyhow::Error>(total)
        };

        let total_stats = remove_unused_files.await?;

        print_summary(self.is_dry_run(), &total_stats);

        Ok(())
    }
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

        try_join_all(
            git_projects
                .iter()
                .map(|git_dir| remove_dead_branches(dry_run, git_dir)),
        )
        .await
        .context("failed to clean up dead branches")?;

        Ok(())
    }
}

fn print_summary(dry_run: bool, total_stats: &cargo::CleanupStats) {
    let mut crates: Vec<_> = total_stats.per_crate.iter().collect();
    crates.sort_by_key(|(_, stat)| std::cmp::Reverse(stat.bytes));

    let color = std::io::stdout().is_terminal();
    let headline_style = Style::new().fg_color(Some(if dry_run {
        AnsiColor::Yellow.into()
    } else {
        AnsiColor::Green.into()
    }));
    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    let headline = if dry_run {
        format!(
            "{} would remove {} files ({}) across {} crates",
            paint(color, "Dry-run:", headline_style),
            paint(color, total_stats.files.to_string(), accent_style),
            paint(color, cargo::format_bytes(total_stats.bytes), accent_style),
            paint(color, crates.len().to_string(), accent_style),
        )
    } else {
        format!(
            "{} {} files ({}) across {} crates",
            paint(color, "Removed", headline_style),
            paint(color, total_stats.files.to_string(), accent_style),
            paint(color, cargo::format_bytes(total_stats.bytes), accent_style),
            paint(color, crates.len().to_string(), accent_style),
        )
    };

    println!("{headline}");

    for (name, stat) in crates.iter().take(20) {
        println!(
            "- {}: {} files ({})",
            paint(color, name.to_string(), accent_style),
            paint(color, stat.files.to_string(), accent_style),
            paint(color, cargo::format_bytes(stat.bytes), accent_style)
        );
    }

    if crates.len() > 20 {
        println!(
            "... and {} more crates",
            paint(color, (crates.len() - 20).to_string(), accent_style)
        );
    }
}

fn paint(enabled: bool, text: impl AsRef<str>, style: Style) -> String {
    if !enabled {
        return text.as_ref().to_string();
    }
    format!("{style}{}{}", text.as_ref(), Reset)
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
    wrap(async move {
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

        let branches = String::from_utf8(branches.stdout)
            .context("failed to parse output of git refs as utf9")?;

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
    })
    .await
    .with_context(|| format!("failed to clean up dead branches in {}", git_dir.display()))
}

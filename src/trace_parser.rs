use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::crate_deps::format_bytes;

/// Result of tracing a cargo build
#[derive(Debug, Default)]
pub struct TraceResult {
    /// Set of artifact paths that were referenced during the build
    pub used_artifacts: HashSet<PathBuf>,
}

/// Parser for cargo build trace output
pub struct TraceParser {
    target_dir: PathBuf,
}

impl TraceParser {
    pub fn new(target_dir: PathBuf) -> Self {
        Self { target_dir }
    }

    /// Run `cmd_str` via `sh -c` with CARGO_LOG trace enabled and collect
    /// every artifact path that cargo's fingerprinting code references.
    /// Spaces and shell quoting work exactly as they would in the shell.
    pub async fn trace(&self, project_dir: &Path, cmd_str: &str) -> Result<TraceResult> {
        println!("🔍 Tracing with command: {cmd_str}...");

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(cmd_str);
        cmd.current_dir(project_dir);
        cmd.env("CARGO_LOG", "cargo::core::compiler::fingerprint=trace")
            .env("CARGO_TERM_COLOR", "always");

        log::debug!("Running command: {:?}", cmd.as_std());

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn command")?;
        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;

        let mut result = TraceResult::default();
        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        // Powerline-style spinner pinned at bottom; cargo output scrolls above it
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
                .template("\u{1b}[44;1;97m {spinner} Tracing \u{1b}[0;34m\u{e0b0}\u{1b}[0;36m  {msg}  \u{1b}[0m")
                .unwrap(),
        );
        spinner.set_message("scanning…");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        let mut total_size: u64 = 0;
        let mut suppress_remaining: u8 = 0;
        let mut last_line_was_blank = false;

        loop {
            tokio::select! {
                stdout_line = stdout_reader.next_line() => {
                    if let Some(line) = stdout_line? { spinner.println(&line); }
                }
                stderr_line = stderr_reader.next_line() => {
                    match stderr_line? {
                        Some(line) => {
                            if let Some(path) = self.extract_artifact_path(&line) {
                                suppress_remaining = 0;
                                if result.used_artifacts.insert(path.clone()) {
                                    if let Ok(meta) = std::fs::metadata(&path) {
                                        total_size += meta.len();
                                    }
                                    spinner.set_message(format!(
                                        "{} artifacts  •  {}",
                                        result.used_artifacts.len(),
                                        format_bytes(total_size),
                                    ));
                                }
                            } else if is_cargo_log_noise(&line, &mut suppress_remaining) {
                                // Swallow CARGO_LOG trace noise silently
                            } else {
                                // Real cargo output (Compiling, warnings, errors, …)
                                let is_blank = line.trim().is_empty();
                                if !(is_blank && last_line_was_blank) {
                                    spinner.println(&line);
                                }
                                last_line_was_blank = is_blank;
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        let status = child.wait().await?;

        spinner.finish_and_clear();

        // Divider to visually separate cargo compile output from the summary
        let width = console::Term::stdout().size().1 as usize;
        let width = if width == 0 { 80 } else { width };
        println!("\x1b[2m{}\x1b[0m", "─".repeat(width));

        println!(
            "✅ Traced \x1b[1;36m{}\x1b[0m artifacts in use  \x1b[2m({})\x1b[0m",
            result.used_artifacts.len(),
            format_bytes(total_size),
        );
        println!();

        if !status.success() {
            anyhow::bail!("Command failed with status: {status}");
        }

        Ok(result)
    }

    /// Extract an artifact path from a single cargo trace log line.
    fn extract_artifact_path(&self, line: &str) -> Option<PathBuf> {
        if !line.contains("mtime") {
            return None;
        }

        // Lines look like:  max output mtime for "…" is "/path/to/libfoo.rlib" …
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() < 2 {
            return None;
        }

        for part in parts.iter().rev() {
            let path = PathBuf::from(part);
            if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy();
                if matches!(ext_str.as_ref(), "rlib" | "rmeta" | "so" | "dylib" | "dll")
                    && path.starts_with(&self.target_dir)
                {
                    return Some(path);
                }
            }
        }

        None
    }
}

/// Returns `true` (and updates `suppress_remaining`) when a stderr line is
/// CARGO_LOG trace noise that should be hidden from the user.
///
/// Three categories of noise from `CARGO_LOG=…=trace`:
///   1. Structured log entries — always contain `cargo::` (the module path).
///   2. `Caused by:` — anyhow error-chain header when cargo can't stat a
///      not-yet-built artifact during fingerprint checks.
///   3. The body after `Caused by:` — e.g. `    No such file or directory`.
///      We suppress exactly `suppress_remaining` non-empty lines after the
///      header (count-based, not indentation-based, so `   Compiling …`
///      lines that also start with spaces are never swallowed).
fn is_cargo_log_noise(line: &str, suppress_remaining: &mut u8) -> bool {
    // Consume a suppressed body line (one line per "Caused by:" header)
    if *suppress_remaining > 0 && !line.trim().is_empty() {
        *suppress_remaining -= 1;
        return true;
    }

    if line.trim().is_empty() {
        return false;
    }

    // Structured CARGO_LOG trace entries always contain the module path
    if line.contains("cargo::") {
        return true;
    }

    // anyhow error-chain header — suppress the next non-empty body line
    if line.trim_start().starts_with("Caused by:") {
        *suppress_remaining = 1;
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_artifact_path() {
        let parser = TraceParser::new(PathBuf::from("/project/target"));

        let line =
            r#"max output mtime for "foo" is "/project/target/debug/deps/libfoo-abc123.rlib" 123s"#;
        assert_eq!(
            parser.extract_artifact_path(line),
            Some(PathBuf::from(
                "/project/target/debug/deps/libfoo-abc123.rlib"
            ))
        );

        let line =
            r#"max dep mtime for "bar" is "/project/target/debug/deps/libbar-xyz789.rmeta" 456s"#;
        assert_eq!(
            parser.extract_artifact_path(line),
            Some(PathBuf::from(
                "/project/target/debug/deps/libbar-xyz789.rmeta"
            ))
        );

        let line = "Some other log line without artifacts";
        assert_eq!(parser.extract_artifact_path(line), None);

        let line = r#"max output mtime is "/other/path/libfoo-abc123.rlib" 123s"#;
        assert_eq!(parser.extract_artifact_path(line), None);
    }
}

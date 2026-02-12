use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Mode for tracing cargo builds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceMode {
    /// Use `cargo check` - faster, only traces .rmeta files
    Check,
    /// Use `cargo build` - complete, traces .rlib and .rmeta files
    Build,
}

/// Feature configuration for cargo builds
#[derive(Debug, Clone, Default)]
pub struct FeatureConfig {
    /// Use all features
    pub all_features: bool,
    /// No default features
    pub no_default_features: bool,
    /// Specific features to enable
    pub features: Option<String>,
}

impl FeatureConfig {
    pub fn auto_detect() -> Self {
        // By default, use the project's current feature configuration
        // (don't pass any feature flags to cargo)
        Self::default()
    }

    pub fn is_default(&self) -> bool {
        !self.all_features && !self.no_default_features && self.features.is_none()
    }
}

/// Result of tracing a cargo build/check
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

    /// Run cargo with trace logging and collect used artifacts
    pub async fn trace(
        &self,
        project_dir: &Path,
        mode: TraceMode,
        profile: Option<&str>,
        feature_config: &FeatureConfig,
    ) -> Result<TraceResult> {
        let feature_desc = if feature_config.all_features {
            " with all features".to_string()
        } else if let Some(ref features) = feature_config.features {
            format!(" with features: {}", features)
        } else if feature_config.no_default_features {
            " with no default features".to_string()
        } else {
            "".to_string()
        };

        println!(
            "🔍 Tracing dependencies using cargo {:?}{}...",
            mode,
            feature_desc
        );

        let mut cmd = Command::new("cargo");
        cmd.current_dir(project_dir);

        // Set the command based on mode
        match mode {
            TraceMode::Check => {
                cmd.arg("check");
            }
            TraceMode::Build => {
                cmd.arg("build");
            }
        }

        // Add feature flags
        if feature_config.all_features {
            cmd.arg("--all-features");
        }
        if feature_config.no_default_features {
            cmd.arg("--no-default-features");
        }
        if let Some(ref features) = feature_config.features {
            cmd.arg("--features").arg(features);
        }

        // Add profile if specified
        if let Some(prof) = profile {
            if prof != "dev" && prof != "debug" {
                cmd.arg("--profile").arg(prof);
            } else if prof == "release" {
                cmd.arg("--release");
            }
        }

        // Set trace logging
        cmd.env("CARGO_LOG", "cargo::core::compiler::fingerprint=trace");

        // Capture both stdout and stderr
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .context("Failed to spawn cargo command")?;

        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Failed to capture stderr")?;

        let mut result = TraceResult::default();

        // Get readers for both stdout and stderr
        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();
        
        println!(); // Add a blank line before cargo output
        
        // Read both streams concurrently
        loop {
            tokio::select! {
                // Pass through stdout (cargo's compilation progress)
                stdout_line = stdout_reader.next_line() => {
                    match stdout_line? {
                        Some(line) => {
                            // Show cargo's progress output directly
                            println!("{}", line);
                        }
                        None => {
                            // stdout closed
                        }
                    }
                }
                // Parse stderr for trace logs
                stderr_line = stderr_reader.next_line() => {
                    match stderr_line? {
                        Some(line) => {
                            if let Some(path) = self.extract_artifact_path(&line) {
                                result.used_artifacts.insert(path);
                            }
                        }
                        None => {
                            // stderr closed, we're done
                            break;
                        }
                    }
                }
            }
        }

        // Wait for command to complete
        let status = child.wait().await?;
        if !status.success() {
            anyhow::bail!("Cargo command failed with status: {}", status);
        }

        println!("\n✅ Trace complete: found {} artifacts in use\n", result.used_artifacts.len());

        Ok(result)
    }

    /// Extract artifact path from a trace log line
    fn extract_artifact_path(&self, line: &str) -> Option<PathBuf> {
        // Look for patterns like:
        // max output mtime for "..." is "/path/to/target/debug/deps/libfoo.rlib"
        // max dep mtime for "..." is "/path/to/target/debug/deps/libbar.rmeta"

        if !line.contains("mtime") {
            return None;
        }

        // Find the last quoted path in the line
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() < 2 {
            return None;
        }

        // The path is typically the last quoted string
        for part in parts.iter().rev() {
            let path = PathBuf::from(part);

            // Check if it's a relevant artifact file
            if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy();
                if ext_str == "rlib"
                    || ext_str == "rmeta"
                    || ext_str == "so"
                    || ext_str == "dylib"
                    || ext_str == "dll"
                {
                    // Verify it's within our target directory
                    if path.starts_with(&self.target_dir) {
                        return Some(path);
                    }
                }
            }
        }

        None
    }

    /// Trace multiple profiles and merge results
    pub async fn trace_profiles(
        &self,
        project_dir: &Path,
        mode: TraceMode,
        profiles: &[String],
        feature_config: &FeatureConfig,
    ) -> Result<TraceResult> {
        let mut merged = TraceResult::default();

        for (idx, profile) in profiles.iter().enumerate() {
            if profiles.len() > 1 {
                println!("📦 Profile {}/{}: {}", idx + 1, profiles.len(), profile);
            }
            
            let result = self
                .trace(project_dir, mode, Some(profile), feature_config)
                .await
                .with_context(|| format!("Failed to trace profile: {}", profile))?;

            merged.used_artifacts.extend(result.used_artifacts);
        }

        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_artifact_path() {
        let parser = TraceParser::new(PathBuf::from("/project/target"));

        // Test rlib extraction
        let line = r#"max output mtime for "foo" is "/project/target/debug/deps/libfoo-abc123.rlib" 123s"#;
        let path = parser.extract_artifact_path(line);
        assert_eq!(
            path,
            Some(PathBuf::from("/project/target/debug/deps/libfoo-abc123.rlib"))
        );

        // Test rmeta extraction
        let line = r#"max dep mtime for "bar" is "/project/target/debug/deps/libbar-xyz789.rmeta" 456s"#;
        let path = parser.extract_artifact_path(line);
        assert_eq!(
            path,
            Some(PathBuf::from("/project/target/debug/deps/libbar-xyz789.rmeta"))
        );

        // Test non-matching line
        let line = "Some other log line without artifacts";
        let path = parser.extract_artifact_path(line);
        assert_eq!(path, None);

        // Test path outside target directory
        let line = r#"max output mtime is "/other/path/libfoo-abc123.rlib" 123s"#;
        let path = parser.extract_artifact_path(line);
        assert_eq!(path, None);
    }
}

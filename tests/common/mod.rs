#![allow(dead_code, unreachable_pub)]

use assert_fs::fixture::{ChildPath, FileWriteStr, PathChild};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git_cmd(dir: impl AsRef<Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(["-c", "commit.gpgsign=false"])
        .args(["-c", "tag.gpgsign=false"])
        .args(["-c", "core.autocrlf=false"])
        .args(["-c", "user.name=Test User"])
        .args(["-c", "user.email=test@example.com"]);
    cmd
}

pub fn cargo_cmd(dir: impl AsRef<Path>) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(dir);
    cmd
}

pub struct TestContext {
    temp_dir: ChildPath,
    
    // To keep the directory alive
    #[allow(dead_code)]
    _root: assert_fs::TempDir,
}

impl TestContext {
    pub fn new() -> Self {
        let root = assert_fs::TempDir::new().expect("Failed to create test root directory");
        let temp_dir = root.child("project");
        
        fs_err::create_dir_all(&temp_dir).expect("Failed to create test working directory");

        Self {
            temp_dir,
            _root: root,
        }
    }

    /// Get the working directory for the test context
    pub fn work_dir(&self) -> &ChildPath {
        &self.temp_dir
    }

    /// Get path to target directory
    pub fn target_dir(&self) -> PathBuf {
        self.temp_dir.join("target")
    }

    /// Initialize a cargo project
    pub fn init_cargo_project(&self, name: &str) {
        cargo_cmd(&self.temp_dir)
            .arg("init")
            .arg("--name")
            .arg(name)
            .arg("--vcs")
            .arg("none")
            .output()
            .expect("Failed to init cargo project");
            
        // Also init git separately
        self.git_init();
    }

    /// Initialize git repo
    pub fn git_init(&self) {
        git_cmd(&self.temp_dir)
            .arg("-c")
            .arg("init.defaultBranch=main")
            .arg("init")
            .output()
            .expect("Failed to init git");
    }

    /// Run `git add`
    pub fn git_add(&self, path: &str) {
        git_cmd(&self.temp_dir)
            .arg("add")
            .arg(path)
            .output()
            .expect("Failed to git add");
    }

    /// Run `git commit`
    pub fn git_commit(&self, message: &str) {
        git_cmd(&self.temp_dir)
            .arg("commit")
            .arg("-m")
            .arg(message)
            .output()
            .expect("Failed to git commit");
    }

    /// Write Cargo.toml with dependencies
    pub fn write_cargo_toml(&self, content: &str) {
        self.temp_dir
            .child("Cargo.toml")
            .write_str(content)
            .expect("Failed to write Cargo.toml");
    }

    /// Write a source file
    pub fn write_src_file(&self, path: &str, content: &str) {
        let file_path = self.temp_dir.child(path);
        if let Some(parent) = file_path.path().parent() {
            fs_err::create_dir_all(parent).ok();
        }
        file_path
            .write_str(content)
            .expect("Failed to write source file");
    }

    /// Run cargo build
    pub fn cargo_build(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = cargo_cmd(&self.temp_dir);
        cmd.arg("build");
        for arg in args {
            cmd.arg(arg);
        }
        cmd.output().expect("Failed to run cargo build")
    }

    /// Run cargo check
    pub fn cargo_check(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = cargo_cmd(&self.temp_dir);
        cmd.arg("check");
        for arg in args {
            cmd.arg(arg);
        }
        cmd.output().expect("Failed to run cargo check")
    }

    /// Count files in target/debug/deps matching pattern
    pub fn count_deps_files(&self, profile: &str, extension: &str) -> usize {
        let deps_dir = self.target_dir().join(profile).join("deps");
        if !deps_dir.exists() {
            return 0;
        }

        fs_err::read_dir(deps_dir)
            .expect("Failed to read deps dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext_str| ext_str == extension)
                    .unwrap_or(false)
            })
            .count()
    }

    /// Get all artifact files in deps directory
    pub fn list_artifacts(&self, profile: &str) -> Vec<PathBuf> {
        let deps_dir = self.target_dir().join(profile).join("deps");
        if !deps_dir.exists() {
            return vec![];
        }

        fs_err::read_dir(deps_dir)
            .expect("Failed to read deps dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext_str| ext_str == "rlib" || ext_str == "rmeta")
                    .unwrap_or(false)
            })
            .map(|e| e.path())
            .collect()
    }
}

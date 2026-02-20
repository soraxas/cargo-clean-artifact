//! Integration tests that verify cargo-clean-artifact is idempotent:
//! running it on a project must not cause subsequent `cargo build` runs
//! to recompile anything.
//!
//! Test matrix
//! ───────────
//! • hello_world            – no external deps  (fast, always runs)
//! • release profile        – same project, --release  (fast)
//! • two profiles together  – build debug + release, clean debug, rebuild both
//! • planted stale artifact – fake .rlib placed in deps/ must be removed
//! • transitive deps        – serde + anyhow (slow, #[ignore])
//! • wasm target            – wasm32-unknown-unknown (requires target, #[ignore])

use std::path::Path;
use std::process::{Command, Output};

use assert_fs::TempDir;

// ── binary under test ──────────────────────────────────────────────────────────

fn cleaner_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_cargo-clean-artifact"))
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Run `cargo build [extra_args]` in `dir`.
/// Panics if the build fails.  Returns the raw Output so callers can inspect stderr.
fn cargo_build(dir: &Path, extra_args: &[&str]) -> Output {
    let out = Command::new("cargo")
        .current_dir(dir)
        .arg("build")
        .args(extra_args)
        .env_remove("CARGO_TARGET_DIR") // avoid shared-cache guard
        .output()
        .expect("failed to spawn cargo");
    if !out.status.success() {
        panic!(
            "cargo build failed in {}:\n{}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out
}

/// Count `   Compiling crate v0.x …` lines in cargo's stderr.
fn compiling_count(output: &Output) -> usize {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|l| l.trim_start().starts_with("Compiling"))
        .count()
}

/// Run the cleaner with --yes (non-interactive) and the given build command.
/// Panics if the tool exits non-zero.
fn run_clean(dir: &Path, build_cmd: &str) -> String {
    let out = Command::new(cleaner_bin())
        .current_dir(dir)
        .args(["--yes", "-c", build_cmd])
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("failed to spawn cleaner");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        panic!(
            "cargo-clean-artifact failed:\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    stdout
}

// ── project generators ────────────────────────────────────────────────────────

fn write_hello_world(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "hello_world"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        r#"fn main() { println!("Hello, world!"); }"#,
    )
    .unwrap();
}

/// A minimal workspace: `main_bin` → `my_lib` (local library).
/// The trace WILL reference `libmy_lib-HASH.rlib` in target/debug/deps/,
/// which populates scan_dirs so other stale artifacts there can be found.
fn write_workspace_with_local_dep(dir: &Path) {
    std::fs::create_dir_all(dir.join("my_lib/src")).unwrap();
    std::fs::create_dir_all(dir.join("main_bin/src")).unwrap();

    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[workspace]
members = ["my_lib", "main_bin"]
resolver = "2"
"#,
    )
    .unwrap();

    std::fs::write(
        dir.join("my_lib/Cargo.toml"),
        r#"[package]
name = "my_lib"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("my_lib/src/lib.rs"),
        r#"pub fn greeting() -> &'static str { "hello" }"#,
    )
    .unwrap();

    std::fs::write(
        dir.join("main_bin/Cargo.toml"),
        r#"[package]
name = "main_bin"
version = "0.1.0"
edition = "2021"

[dependencies]
my_lib = { path = "../my_lib" }
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("main_bin/src/main.rs"),
        r#"fn main() { println!("{}", my_lib::greeting()); }"#,
    )
    .unwrap();
}
fn write_with_deps(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "with_deps"
version = "0.1.0"
edition = "2021"

[dependencies]
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
anyhow      = "1"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        r#"use anyhow::Result;
use serde::Serialize;

#[allow(dead_code)]
#[derive(Serialize)]
struct Point { x: i32, y: i32 }

fn main() -> Result<()> {
    println!("{}", serde_json::to_string(&Point { x: 1, y: 2 }).unwrap_or_default());
    Ok(())
}
"#,
    )
    .unwrap();
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Simplest possible project: no external deps.
/// Cleaning then rebuilding must produce zero `Compiling` lines.
#[test]
fn test_no_recompile_hello_world() {
    let tmp = TempDir::new().unwrap();
    write_hello_world(tmp.path());

    cargo_build(tmp.path(), &[]);
    run_clean(tmp.path(), "cargo build");

    let rebuild = cargo_build(tmp.path(), &[]);
    assert_eq!(
        compiling_count(&rebuild),
        0,
        "unexpected recompilation after clean:\n{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
}

/// Release profile: the final binary must be protected even though it is not
/// referenced as a dependency in the trace.
#[test]
fn test_no_recompile_release_profile() {
    let tmp = TempDir::new().unwrap();
    write_hello_world(tmp.path());

    cargo_build(tmp.path(), &["--release"]);
    run_clean(tmp.path(), "cargo build --release");

    let rebuild = cargo_build(tmp.path(), &["--release"]);
    assert_eq!(
        compiling_count(&rebuild),
        0,
        "unexpected recompilation after release clean:\n{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
}

/// Build both debug and release, then clean only the debug profile.
/// Neither profile should require recompilation afterwards.
#[test]
fn test_clean_debug_leaves_release_intact() {
    let tmp = TempDir::new().unwrap();
    write_hello_world(tmp.path());

    cargo_build(tmp.path(), &[]); // debug
    cargo_build(tmp.path(), &["--release"]); // release

    // Clean only the debug profile
    run_clean(tmp.path(), "cargo build");

    let debug_rebuild = cargo_build(tmp.path(), &[]);
    assert_eq!(
        compiling_count(&debug_rebuild),
        0,
        "debug recompiled after debug-only clean:\n{}",
        String::from_utf8_lossy(&debug_rebuild.stderr)
    );

    let release_rebuild = cargo_build(tmp.path(), &["--release"]);
    assert_eq!(
        compiling_count(&release_rebuild),
        0,
        "release recompiled after debug-only clean (should be untouched):\n{}",
        String::from_utf8_lossy(&release_rebuild.stderr)
    );
}

/// Running clean a second time on an already-clean project must find nothing.
#[test]
fn test_second_clean_is_noop() {
    let tmp = TempDir::new().unwrap();
    write_hello_world(tmp.path());

    cargo_build(tmp.path(), &[]);
    run_clean(tmp.path(), "cargo build"); // first clean

    // Second clean — should report "No unused artifacts"
    let second = run_clean(tmp.path(), "cargo build");
    assert!(
        second.contains("No unused artifacts") || second.contains("0 files"),
        "second clean should find nothing, got:\n{second}"
    );

    // And rebuilding is still clean
    let rebuild = cargo_build(tmp.path(), &[]);
    assert_eq!(
        compiling_count(&rebuild),
        0,
        "unexpected recompilation after second clean:\n{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
}

/// A fake `.rlib` planted in deps/ must be removed by the clean tool.
/// Uses a workspace with a local library dep so the trace actually populates
/// scan_dirs (a pure binary with no deps produces no traceable .rlib/.rmeta).
#[test]
fn test_stale_artifact_is_removed() {
    let tmp = TempDir::new().unwrap();
    write_workspace_with_local_dep(tmp.path());

    cargo_build(tmp.path(), &["--workspace"]);

    // Plant a fake stale artifact alongside the real ones
    let fake = tmp
        .path()
        .join("target/debug/deps/libstale_crate-deadbeef00000000.rlib");
    std::fs::write(&fake, b"not a real rlib").unwrap();
    assert!(fake.exists(), "fake artifact should exist before clean");

    run_clean(tmp.path(), "cargo build --workspace");

    assert!(
        !fake.exists(),
        "fake stale artifact should have been removed"
    );
}

/// Test if cleaning requires another re-compile
/// When the trace command itself causes recompilation (e.g. because a previous
/// clean removed artifacts), cargo skips the fingerprint mtime log for the
/// crates it just built in the same session.  The cleaner never sees those
/// files in the trace and incorrectly deletes them, forcing another recompile.
///
/// Scenario:
///   1. Build workspace (my_lib + main_bin).
///   2. Delete libmy_lib's .rlib/.rmeta to force a recompile.
///   3. Run cleaner — it traces `cargo build`, which recompiles my_lib.
///      BUG: the freshly compiled libmy_lib.rlib is not in the mtime trace,
///      so the cleaner deletes it.
///   4. Next `cargo build` should be a no-op — but it isn't (it recompiles my_lib).
#[test]
fn test_no_recompile_after_clean_recompile() {
    let tmp = TempDir::new().unwrap();
    write_workspace_with_local_dep(tmp.path());

    // Step 1: initial full build
    cargo_build(tmp.path(), &["--workspace"]);

    // Step 2: delete libmy_lib artifacts to force recompilation on next build
    let deps_dir = tmp.path().join("target/debug/deps");
    let lib_artifacts: Vec<_> = std::fs::read_dir(&deps_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("libmy_lib")
        })
        .collect();
    assert!(
        !lib_artifacts.is_empty(),
        "should have libmy_lib artifacts after initial build"
    );
    for f in &lib_artifacts {
        std::fs::remove_file(f).unwrap();
    }

    // Step 3: run cleaner — this traces `cargo build --workspace`, which
    // recompiles my_lib.  The freshly compiled libmy_lib.rlib is NOT logged
    // in cargo's fingerprint mtime output (cargo skips the check for deps
    // compiled in the same session), so the cleaner incorrectly deletes it.
    run_clean(tmp.path(), "cargo build --workspace");

    // Step 4: rebuild should find everything intact → 0 Compiling lines.
    let rebuild = cargo_build(tmp.path(), &["--workspace"]);
    assert_eq!(
        compiling_count(&rebuild),
        0,
        "BUG: recompiled after clean (freshly compiled lib was incorrectly deleted):\n{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
}

/// Cleaning with a wasm target must not prevent a subsequent wasm build from
/// finding its artifacts (no recompilation).
#[test]
#[ignore = "requires wasm32-unknown-unknown target (rustup target add wasm32-unknown-unknown)"]
fn test_no_recompile_wasm_target() {
    let tmp = TempDir::new().unwrap();
    write_hello_world(tmp.path());

    let wasm_args = &["--target", "wasm32-unknown-unknown"];
    let wasm_cmd = "cargo build --target wasm32-unknown-unknown";

    cargo_build(tmp.path(), wasm_args);
    run_clean(tmp.path(), wasm_cmd);

    let rebuild = cargo_build(tmp.path(), wasm_args);
    assert_eq!(
        compiling_count(&rebuild),
        0,
        "unexpected wasm recompilation after clean:\n{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
}

/// Wasm clean must not disturb a pre-existing native debug build.
#[test]
#[ignore = "requires wasm32-unknown-unknown target (rustup target add wasm32-unknown-unknown)"]
fn test_wasm_clean_leaves_native_intact() {
    let tmp = TempDir::new().unwrap();
    write_hello_world(tmp.path());

    cargo_build(tmp.path(), &[]); // native debug
    cargo_build(tmp.path(), &["--target", "wasm32-unknown-unknown"]); // wasm

    // Clean only the wasm build
    run_clean(tmp.path(), "cargo build --target wasm32-unknown-unknown");

    // Native rebuild must be instant
    let native_rebuild = cargo_build(tmp.path(), &[]);
    assert_eq!(
        compiling_count(&native_rebuild),
        0,
        "native build recompiled after wasm-only clean:\n{}",
        String::from_utf8_lossy(&native_rebuild.stderr)
    );

    // Wasm rebuild must also be instant
    let wasm_rebuild = cargo_build(tmp.path(), &["--target", "wasm32-unknown-unknown"]);
    assert_eq!(
        compiling_count(&wasm_rebuild),
        0,
        "wasm build recompiled after wasm-only clean:\n{}",
        String::from_utf8_lossy(&wasm_rebuild.stderr)
    );
}

/// Full cycle with transitive dependencies (serde uses proc-macro2, syn, …).
/// Marked slow because it downloads crates on a cold cache.
#[test]
#[ignore = "slow: downloads serde + anyhow from crates.io on a cold cache"]
fn test_no_recompile_with_transitive_deps() {
    let tmp = TempDir::new().unwrap();
    write_with_deps(tmp.path());

    cargo_build(tmp.path(), &[]);
    run_clean(tmp.path(), "cargo build");

    let rebuild = cargo_build(tmp.path(), &[]);
    assert_eq!(
        compiling_count(&rebuild),
        0,
        "unexpected recompilation after clean with transitive deps:\n{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
}

/// Same as above but for the release profile.
#[test]
#[ignore = "slow: downloads serde + anyhow from crates.io on a cold cache"]
fn test_no_recompile_with_transitive_deps_release() {
    let tmp = TempDir::new().unwrap();
    write_with_deps(tmp.path());

    cargo_build(tmp.path(), &["--release"]);
    run_clean(tmp.path(), "cargo build --release");

    let rebuild = cargo_build(tmp.path(), &["--release"]);
    assert_eq!(
        compiling_count(&rebuild),
        0,
        "unexpected recompilation after release clean with transitive deps:\n{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
}

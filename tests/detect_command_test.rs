use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use filetime::{FileTime, set_file_mtime};

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [features]\ndefault = [\"webgpu\"]\nwebgpu = []\nbrp = []\n\
         [profile.wasm-dev]\ninherits = \"dev\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    // A replay would execute this script and fail, leaving a marker behind.
    fs::write(
        dir.path().join("build.rs"),
        "fn main() { std::fs::write(\"BUILD_WAS_RUN\", \"yes\").unwrap(); panic!(\"must not build\"); }",
    )
    .unwrap();
    dir
}

fn fingerprint(root: &Path, path: &str, json: &str, time: i64) -> PathBuf {
    let path = root.join("target").join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, json).unwrap();
    set_file_mtime(&path, FileTime::from_unix_time(time, 0)).unwrap();
    // Minimal rustc depfile: ownership is checked against the manifest's source.
    let filename = path.file_name().unwrap().to_str().unwrap();
    if let Some(name) = filename
        .strip_prefix("bin-")
        .or_else(|| filename.strip_prefix("lib-"))
    {
        let name = name.trim_end_matches(".json");
        let hash = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .rsplit_once('-')
            .unwrap()
            .1;
        let profile = path.parent().unwrap().parent().unwrap().parent().unwrap();
        let deps = profile.join("deps");
        fs::create_dir_all(&deps).unwrap();
        let source = match name {
            "helper" => "helper/src/lib.rs",
            "two" => "tool/src/bin/two.rs",
            _ if filename.starts_with("lib-") => "src/lib.rs",
            _ => "src/main.rs",
        };
        fs::write(
            deps.join(format!("{}-{hash}.d", name.replace('-', "_"))),
            format!("artifact: {source}\n\n{source}:\n"),
        )
        .unwrap();
    }
    path
}

const WASM: &str =
    r#"{"features":"[\"brp\", \"webgpu\"]","rustflags":[],"compile_kind":14682669768258224367}"#;
const NATIVE: &str = r#"{"features":"[\"default\", \"webgpu\"]","rustflags":[],"compile_kind":0}"#;

fn detect(dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-clean-artifact"))
        .args(["--detect"])
        .arg(dir)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET")
        .output()
        .unwrap()
}

fn commands(out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn detects_wasm_features_and_custom_profile_without_building_or_cleaning() {
    let dir = project();
    let path = fingerprint(
        dir.path(),
        "wasm32-unknown-unknown/wasm-dev/.fingerprint/demo-abc/bin-demo.json",
        WASM,
        100,
    );
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    let out = detect(dir.path());
    assert_eq!(
        commands(&out),
        [
            "cargo build --target wasm32-unknown-unknown --profile wasm-dev --no-default-features --features brp,webgpu"
        ]
    );
    assert!(!dir.path().join("BUILD_WAS_RUN").exists());
    assert_eq!(fs::read_to_string(&path).unwrap(), WASM);
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), before);
    assert!(!dir.path().join("Cargo.lock").exists());
}

#[test]
fn sorts_newest_first_and_deduplicates_matching_library_and_binary() {
    let dir = project();
    fs::write(dir.path().join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-old/bin-demo.json",
        NATIVE,
        10,
    );
    fingerprint(
        dir.path(),
        "wasm32-unknown-unknown/wasm-dev/.fingerprint/demo-abc/bin-demo.json",
        WASM,
        20,
    );
    fingerprint(
        dir.path(),
        "wasm32-unknown-unknown/wasm-dev/.fingerprint/demo-def/lib-demo.json",
        WASM,
        21,
    );
    let found = commands(&detect(dir.path()));
    assert_eq!(found.len(), 2);
    assert!(found[0].contains("--target wasm32-unknown-unknown --profile wasm-dev"));
    assert_eq!(found[1], "cargo build --profile dev --features webgpu");
}

#[test]
fn ignores_build_scripts_dependencies_and_nested_target_caches() {
    let dir = project();
    fingerprint(
        dir.path(),
        "wasm-dev/.fingerprint/demo-host/build-script-build-script-build.json",
        WASM,
        40,
    );
    fingerprint(
        dir.path(),
        "debug/.fingerprint/dependency-abc/lib-dependency.json",
        NATIVE,
        30,
    );
    fingerprint(
        dir.path(),
        "other-cache/debug/.fingerprint/demo-abc/bin-demo.json",
        NATIVE,
        20,
    );
    fs::write(dir.path().join("target/other-cache/CACHEDIR.TAG"), "cache").unwrap();
    fingerprint(
        dir.path(),
        "release/.fingerprint/demo-abc/bin-demo.json",
        NATIVE,
        10,
    );
    assert_eq!(
        commands(&detect(dir.path())),
        ["cargo build --profile release --features webgpu"]
    );
}

#[test]
fn malformed_fingerprint_does_not_hide_valid_candidates() {
    let dir = project();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-bad/bin-demo.json",
        "{partial",
        50,
    );
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-good/bin-demo.json",
        NATIVE,
        10,
    );
    assert_eq!(commands(&detect(dir.path())).len(), 1);
}

#[test]
fn missing_cache_reports_no_candidates_without_building() {
    let dir = project();
    let out = detect(dir.path());
    assert!(commands(&out).is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No build commands"));
    assert!(!dir.path().join("target").exists());
}

#[test]
fn preserves_recorded_rustflags_as_shell_safe_encoded_flags() {
    let dir = project();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-abc/bin-demo.json",
        r#"{"features":"[]","rustflags":["--cfg","message=\"don't $(touch OWNED)\""],"compile_kind":0}"#,
        10,
    );
    let found = commands(&detect(dir.path()));
    assert_eq!(found.len(), 1);
    assert!(found[0].starts_with("CARGO_ENCODED_RUSTFLAGS="));
    assert!(found[0].ends_with("cargo build --profile dev --no-default-features"));
    assert!(!dir.path().join("OWNED").exists());
    // Evaluate only the emitted environment assignment, replacing cargo with printf.
    let assignment = found[0].split(" cargo build").next().unwrap();
    let out = Command::new("sh")
        .args([
            "-c",
            &format!("{assignment} sh -c 'printf %s \"$CARGO_ENCODED_RUSTFLAGS\"'"),
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "--cfg\u{1f}message=\"don't $(touch OWNED)\""
    );
    assert!(!dir.path().join("OWNED").exists());
}

#[test]
fn detection_cannot_be_combined_with_an_executed_command() {
    let dir = project();
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-clean-artifact"))
        .args(["--detect", "-c", "touch SHOULD_NOT_RUN"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!dir.path().join("SHOULD_NOT_RUN").exists());
}

#[test]
fn package_directory_does_not_suggest_builds_of_other_workspace_members() {
    let dir = project();
    let manifest = dir.path().join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[workspace]\nmembers = [\"helper\"]\ndefault-members = [\"helper\"]\n");
    fs::write(manifest, text).unwrap();
    fs::create_dir_all(dir.path().join("helper/src")).unwrap();
    fs::write(
        dir.path().join("helper/Cargo.toml"),
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("helper/src/lib.rs"), "pub fn helper() {}\n").unwrap();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/helper-abc/lib-helper.json",
        r#"{"features":"[]","rustflags":[],"compile_kind":0}"#,
        20,
    );
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-abc/bin-demo.json",
        NATIVE,
        10,
    );
    assert_eq!(
        commands(&detect(dir.path())),
        ["cargo build --profile dev --features webgpu --package demo@0.1.0"]
    );
    assert_eq!(
        commands(&detect(&dir.path().join("helper"))),
        ["cargo build --profile dev --no-default-features --package helper@0.1.0"]
    );
}

#[test]
fn virtual_workspace_commands_select_the_matching_package_and_binary() {
    let dir = project();
    fs::remove_file(dir.path().join("Cargo.toml")).unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"tool\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("tool/src/bin")).unwrap();
    fs::write(
        dir.path().join("tool/Cargo.toml"),
        "[package]\nname = \"my-tool\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("tool/src/bin/one.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.path().join("tool/src/bin/two.rs"), "fn main() {}\n").unwrap();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/my-tool-abc/bin-two.json",
        r#"{"features":"[]","rustflags":[],"compile_kind":0}"#,
        10,
    );
    assert_eq!(
        commands(&detect(dir.path())),
        ["cargo build --profile dev --no-default-features --package my-tool@0.1.0 --bin two"]
    );
}

#[test]
fn respects_configured_target_directory_without_requiring_cleanup_permission() {
    let dir = project();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-abc/bin-demo.json",
        NATIVE,
        10,
    );
    fs::rename(dir.path().join("target"), dir.path().join("cache")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-clean-artifact"))
        .arg("--detect")
        .arg(dir.path())
        .env("CARGO_TARGET_DIR", dir.path().join("cache"))
        .output()
        .unwrap();
    assert_eq!(
        commands(&out),
        ["cargo build --profile dev --features webgpu"]
    );
}

#[test]
fn same_named_dependency_is_not_mistaken_for_the_root_library() {
    let dir = project();
    fs::remove_file(dir.path().join("build.rs")).unwrap();
    fs::remove_file(dir.path().join("src/main.rs")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn demo() { old_demo::old(); }\n",
    )
    .unwrap();
    let manifest = dir.path().join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("0.1.0", "0.2.0");
    text.push_str("\n[dependencies]\nold_demo = { package = \"demo\", path = \"old\", features = [\"legacy\"] }\n");
    fs::write(manifest, text).unwrap();
    fs::create_dir_all(dir.path().join("old/src")).unwrap();
    fs::write(dir.path().join("old/Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nlegacy = []\n").unwrap();
    fs::write(dir.path().join("old/src/lib.rs"), "pub fn old() {}\n").unwrap();
    let built = Command::new("cargo")
        .args(["build", "--offline"])
        .current_dir(dir.path())
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    // The fixture is now built; a detection replay must fail this assertion.
    fs::write(
        dir.path().join("src/lib.rs"),
        "compile_error!(\"do not replay\");\n",
    )
    .unwrap();
    assert_eq!(
        commands(&detect(dir.path())),
        ["cargo build --profile dev --features webgpu"]
    );

    // At a virtual workspace root, name-only --package demo is ambiguous with
    // the dependency. Verify the inferred selector identifies the member version.
    let workspace = tempfile::tempdir().unwrap();
    let member = workspace.path().join("member");
    fs::rename(dir.path(), &member).unwrap();
    fs::rename(member.join("old"), workspace.path().join("old")).unwrap();
    let manifest = member.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("path = \"old\"", "path = \"../old\"");
    fs::write(manifest, text).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nexclude = [\"old\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        member.join("src/lib.rs"),
        "pub fn demo() { old_demo::old(); }\n",
    )
    .unwrap();
    let built = Command::new("cargo")
        .args(["build", "--offline", "--package", "demo@0.2.0"])
        .current_dir(workspace.path())
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert_eq!(
        commands(&detect(workspace.path())),
        ["cargo build --profile dev --features webgpu --package demo@0.2.0"]
    );
}

#[test]
fn source_ownership_accepts_escaped_spaces_in_depfiles() {
    let dir = project();
    fs::remove_file(dir.path().join("src/main.rs")).unwrap();
    fs::write(dir.path().join("src/a root.rs"), "pub fn demo() {}\n").unwrap();
    let manifest = dir.path().join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap() + "\n[lib]\npath = \"src/a root.rs\"\n";
    fs::write(manifest, text).unwrap();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-abc/lib-demo.json",
        NATIVE,
        10,
    );
    fs::write(
        dir.path().join("target/debug/deps/demo-abc.d"),
        "artifact: src/a\\ root.rs\n",
    )
    .unwrap();
    assert_eq!(
        commands(&detect(dir.path())),
        ["cargo build --profile dev --features webgpu"]
    );
}

#[test]
fn missing_depfile_cannot_establish_package_ownership() {
    let dir = project();
    fingerprint(
        dir.path(),
        "debug/.fingerprint/demo-abc/bin-demo.json",
        NATIVE,
        10,
    );
    fs::remove_file(dir.path().join("target/debug/deps/demo-abc.d")).unwrap();
    assert!(commands(&detect(dir.path())).is_empty());
}

#[test]
fn dynamic_libraries_use_unhashed_depfiles_without_reviving_old_features() {
    for kind in ["cdylib", "dylib"] {
        let dir = project();
        fs::remove_file(dir.path().join("build.rs")).unwrap();
        fs::remove_file(dir.path().join("src/main.rs")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        let manifest = dir.path().join("Cargo.toml");
        let text = fs::read_to_string(&manifest).unwrap()
            + &format!("\n[lib]\ncrate-type = [\"{kind}\"]\n");
        fs::write(manifest, text).unwrap();
        for features in ["webgpu", "brp"] {
            let built = Command::new("cargo")
                .args([
                    "build",
                    "--offline",
                    "--no-default-features",
                    "--features",
                    features,
                ])
                .current_dir(dir.path())
                .env_remove("CARGO_TARGET_DIR")
                .output()
                .unwrap();
            assert!(
                built.status.success(),
                "{}",
                String::from_utf8_lossy(&built.stderr)
            );
        }
        let found = commands(&detect(dir.path()));
        assert_eq!(
            found,
            ["cargo build --profile dev --no-default-features --features brp"],
            "{kind}"
        );
    }
}

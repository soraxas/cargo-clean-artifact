[![CI](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/ci.yml/badge.svg)](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/soraxas/cargo-clean-artifact/graph/badge.svg?token=Mk7JwiMg76)](https://codecov.io/gh/soraxas/cargo-clean-artifact)
[![Release](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/release.yml/badge.svg)](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/release.yml)

# cargo-clean-artifact

Prune stale Rust build artifacts from `target/` by tracing which files are
actually referenced during a build. Infer candidate build commands from existing
fingerprints without replaying a build.

Run your build command once; `cargo-clean-artifact` captures the artifact
paths cargo's fingerprint engine logs, removes everything in
`target/{profile}/deps/` that was **not** referenced, and also cleans up
stale incremental compilation sessions — leaving only what the next build
actually needs, so it requires zero recompilation.

## Installation

```sh
cargo install --git https://github.com/soraxas/cargo-clean-artifact
# or
mise use -g github:soraxas/cargo-clean-artifact
```

## Usage

```sh
cargo clean-artifact [OPTIONS] [DIR]
```

Use `--detect` to print inferred commands and exit without building or cleaning.
Without `-c`, an interactive terminal groups detected commands by their artifact
folder, for example `debug/` or `wasm32-unknown-unknown/wasm-dev/`. The folder with
the newest fingerprint comes first, with newer commands first within each folder.
Detected commands are blue, with bold white folder headings on blue bands;
the instruction is cyan, presets are grey, and the selection is green.
Folder headings include a quick size estimate such as `~6.63 GiB total`, measured
once from file metadata when opening the picker. This is the whole folder's size,
not the space a particular command can reclaim. On Unix it counts allocated
blocks and deduplicates hardlinks within each folder; elsewhere it sums file
lengths. Symlinks are not followed. An incomplete scan shows a lower bound (`≥`)
or `size unavailable`. `--detect` does not scan folder sizes.
Each detected command starts with a relative age such as `[2h ago]`; the newest
overall is marked `[newest · 2h ago]`. Ages use the newest matching fingerprint's
modification time, not a recorded last invocation: a no-op build may not update
them. Rows show the full Cargo command with feature values highlighted in yellow,
including on the selected row. Long commands are clipped to the terminal width.
Common presets and custom input have their
own group after the detected folders. Selecting a
command executes it to trace artifacts for cleanup. Press `v` to view the full
selected command, or use left/right arrows to scroll a long command while its age
stays visible. Changing selection resets horizontal scrolling. For non-interactive
cleanup,
provide `-c` / `--command`; it is executed via `sh -c`, so shell quoting, pipes,
and spaces in arguments work normally.

```sh
# Infer commands from an existing cache; no build or cleanup
cargo clean-artifact --detect
cargo clean-artifact --detect /path/to/embodx

# Choose an inferred command or a preset interactively, then trace it
cargo clean-artifact

# Standard debug build
cargo clean-artifact -c "cargo build"

# Release profile
cargo clean-artifact -c "cargo build --release"

# Specific features / target
cargo clean-artifact -c "cargo build --features serde --target wasm32-unknown-unknown"

# trunk (WASM bundler)
cargo clean-artifact -c "trunk build"

# mise task
cargo clean-artifact -c "mise run wasm-dev-build"

# Skip the confirmation prompt and remove immediately
cargo clean-artifact -c "cargo build" -y

# Verbose: show debug log (target dir, exact command, etc.)
cargo clean-artifact -c "cargo build" -v
```

### Options

| Flag | Description |
|------|-------------|
| `--detect` | Print inferred commands, newest first, without building or cleaning |
| `-c, --command <CMD>` | Build command to execute and trace (interactive picker if omitted) |
| `-y, --yes` | Remove files without confirmation |
| `--dry-run` | Preview what would be removed (default) |
| `-n, --trace-stats <N>` | Show top N largest in-use artifacts (default: 5) |
| `-v, --verbose` | Debug logging (target dir, command, …) |
| `--allow-shared-target-dir` | Allow cleaning a shared/global `CARGO_TARGET_DIR` |
| `[DIR]` | Directory to clean (default: `.`) |

### Recursive mode

```bash
# Find every cargo project (Cargo.toml + target/) under ~/work and pick one
cargo clean-artifact -r ~/work
```

Projects are listed largest `target/` first. `Enter` runs the normal
trace-based cleanup for the selected project and returns to the list;
`x` removes that project's whole `target/` (after confirmation); `q` quits.

### Command detection

`--detect` reads Cargo's `.fingerprint` JSON files and matching `deps/*.d` source
dependency files, and uses
`cargo metadata --no-deps --offline` to identify the selected package and target
directory. It does not execute a build, run build scripts, or delete artifacts.
At a virtual workspace root it examines workspace members and includes package
selectors in the output. From a package directory it only suggests that package.
Multi-package workspaces include an explicit package selector so workspace
`default-members` cannot redirect the inferred command to another package.
Selectors include the version to distinguish dependencies with the same name.

For example, an EmbodX fingerprint can produce:

```sh
cargo build --target wasm32-unknown-unknown --profile wasm-dev \
  --no-default-features --features brp,webgpu
```

The target and profile come from the directory layout (`debug` maps to Cargo's
`dev` profile). Features come from the fingerprint. Recorded extra compiler flags
are preserved with a shell-quoted `CARGO_ENCODED_RUSTFLAGS` assignment; its arguments
are separated by Cargo's unit-separator character. Duplicate commands are combined,
and the newest matching fingerprint determines their order. Host build scripts,
external dependencies, and nested target caches are excluded.
Source depfiles distinguish workspace packages from dependencies that happen to
share their name. Fingerprints with missing or unrecognized depfiles are skipped.
Native dynamic libraries may use an unhashed depfile; detection accepts it only
when it does not postdate the candidate fingerprint. This relies on preserved
file timestamps, as does newest-first ordering.

These are **inferred build selections**, not recovered original command lines.
Fingerprints do not identify the original wrapper, `build` versus `check`, original
package grouping, `--locked`, toolchain selection, or arbitrary environment
variables. Features may include transitively enabled features. Current Cargo
configuration and manifests still apply, and older fingerprints may describe
configurations that are no longer supported. Malformed or unsupported fingerprints
are skipped (`-v` shows why). Command detection alone does not establish which
artifacts are safe to delete; cleanup still executes and traces a selected command.

Standard output contains only commands, one per line; notices go to standard
error. If no usable fingerprints exist, detection exits successfully without
printing commands. `--detect` cannot be combined with `-c`, `--yes`, or `--dry-run`.

## How It Works

1. **Trace**: Runs your build command with
   `CARGO_LOG=cargo::core::compiler::fingerprint=trace` and captures every
   artifact path that cargo's fingerprint engine references (`.rlib`,
   `.rmeta`, `.so`, `.dylib`, `.dll`, `.wasm`, …).

2. **Scan `deps/`**: Collects all files in the `deps/` directories that
   appeared in the trace (e.g. `target/debug/deps/`,
   `target/wasm32-unknown-unknown/wasm-dev/deps/`). Files outside those
   directories are never touched.

3. **Scan `incremental/`**: For each profile, groups the incremental
   compilation session directories by crate name and keeps only the
   most-recently-modified session per crate. All older sessions are
   marked for removal.

4. **Protect output artifacts**: Files sitting directly in `target/{profile}/`
   (the final linked binary, `.rlib`, `.wasm`, etc.) are never removed, even
   if they didn't appear in the trace.

5. **Remove** (step-by-step confirmation): Prompts separately for stale
   `deps/` artifacts and stale incremental sessions, then asks for a final
   combined confirmation before touching anything.

### Profile / target isolation

The tool only cleans directories it actually observed in the trace. If you
run `cargo clean-artifact -c "trunk build"` it will only scan the wasm
profile's `deps/` and `incremental/` folders, leaving `target/debug/`
completely untouched.

### Idempotency

Running the tool twice in a row is safe: the second run will report
"No unused artifacts found" and the subsequent build will not recompile
anything.

## Example Output

```text
🔍 Tracing with custom command: cargo build --release...
   Compiling serde v1.0.219
   Compiling my-crate v0.3.0 (...)
    Finished `release` profile [optimized] target(s) in 12.34s
────────────────────────────────────────────────────────────────
✅ Traced 247 artifacts in use  (312.50 MiB)

📂 Build profiles: release

📦 Top 5 in-use artifacts (247 total):
    1. release libjiff-69bb3ab00abe931c.rlib (8.25 MiB) ← my-crate
    2. release libsyn-f41f8c7f54cf32d8.rlib (8.25 MiB) ← my-crate
    3. release libtokio-ffc43fdca28ca7f4.rmeta (7.34 MiB) ← my-crate
    … and 244 more in-use files

By profile:
  release: [312.50 MiB kept / 493.00 MiB total dir]

🗑  Top files to remove: ▶
  🗑  release libtokio-oldabcd1234.rlib (8.85 MiB)
  🗑  release libsyn-old5678efgh.rlib (8.25 MiB)
  … and 37 more files

❯ Remove 42 stale artifact files (180.23 MiB)? [y/N]: y

🗂  Stale incremental sessions:
  🗑  my_crate-1893d467y0y5b (45.10 MiB)
  🗑  serde-3743rt092g0bi (12.30 MiB)
  … and 3 more stale sessions
❯ Remove 5 stale incremental dirs (89.40 MiB)? [y/N]: y

❯ Remove 42 files + 5 stale incremental dirs (269.63 MiB)? [y/N]: y
```

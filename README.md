[![CI](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/ci.yml/badge.svg)](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/soraxas/cargo-clean-artifact/graph/badge.svg?token=Mk7JwiMg76)](https://codecov.io/gh/soraxas/cargo-clean-artifact)
[![Release](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/release.yml/badge.svg)](https://github.com/soraxas/cargo-clean-artifact/actions/workflows/release.yml)

# cargo-clean-artifact

Prune stale Rust build artifacts from `target/` by tracing which files are
actually referenced during a build — not guessing.

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
cargo clean-artifact -c <BUILD_COMMAND> [OPTIONS]
```

`-c` / `--command` is **required**. Supply whatever command you normally use
to build your project. It is executed via `sh -c`, so shell quoting, pipes,
and spaces in arguments all work normally.

```sh
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
| `-c, --command <CMD>` | Build command to trace (**required**) |
| `-y, --yes` | Remove files without confirmation |
| `--dry-run` | Preview what would be removed (default) |
| `-n, --trace-stats <N>` | Show top N largest in-use artifacts (default: 5) |
| `-v, --verbose` | Debug logging (target dir, command, …) |
| `--allow-shared-target-dir` | Allow cleaning a shared/global `CARGO_TARGET_DIR` |
| `[DIR]` | Directory to clean (default: `.`) |

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

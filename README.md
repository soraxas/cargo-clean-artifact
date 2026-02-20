# cargo-clean-artifact

Prune stale Rust build artifacts from `target/` by tracing which files are
actually referenced during a build — not guessing.

Run your build command once; `cargo-clean-artifact` captures the artifact
paths cargo logs, removes everything in `target/{profile}/deps/` that was
**not** referenced, and leaves everything that was needed intact so the next
build requires zero recompilation.

## Installation

```sh
cargo install cargo-clean-artifact
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
| `-v, --verbose` | Debug logging (target dir, command, …) |
| `--allow-shared-target-dir` | Allow cleaning a shared/global `CARGO_TARGET_DIR` |
| `[DIR]` | Directory to clean (default: `.`) |

## How It Works

1. **Trace**: Runs your build command with
   `CARGO_LOG=cargo::core::compiler::fingerprint=trace` and captures every
   artifact path that cargo's fingerprint engine references (`.rlib`,
   `.rmeta`, `.so`, `.dylib`, `.dll`, …).

2. **Scan**: Collects all files in the `deps/` directories that appeared in
   the trace (e.g. `target/debug/deps/`, `target/wasm32-unknown-unknown/wasm-dev/deps/`).
   Files outside those directories are never touched.

3. **Protect output artifacts**: Files sitting directly in `target/{profile}/`
   (the final linked binary, `.rlib`, `.wasm`, etc.) are never removed, even
   if they didn't appear in the trace.

4. **Remove**: Everything in the scanned `deps/` directories that was **not**
   referenced is deleted. Only profiles/targets that appeared in your build
   are touched — a wasm build will never clean your native `debug/` artifacts.

### Profile / target isolation

The tool only cleans directories it actually observed in the trace. If you
run `cargo clean-artifact -c "trunk build"` it will only scan the wasm
profile's `deps/` folder, leaving `target/debug/` completely untouched.

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

📊 Summary: 42 files (180.23 MiB) can be removed  •  312.50 MiB in use

By profile:
  release: 42 files (180.23 MiB)

Top files to remove:
  1. release libserde-old1234abcd.rlib (45.10 MiB)
  2. release libsyn-old5678efgh.rlib  (38.70 MiB)
  ...

❯ Remove 42 files (180.23 MiB)? [y/N]:
```

## License

MIT

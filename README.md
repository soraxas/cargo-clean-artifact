# cargo-clean-artifact

Clean old cargo build artifacts and dependencies that are no longer used by any workspace features.

## Installation

```sh
cargo install cargo-clean-artifact
```

## Usage

```sh
cargo clean-artifact [OPTIONS]
```

### Cleaning Modes

**1. Default Mode (Fast, ~50-60% accurate)**
```bash
cargo clean-artifact
```
Uses `.d` dependency files to detect unused artifacts. Fastest but may miss some stray files.

**2. Check Mode (Recommended, ~80-90% accurate)**
```bash
cargo clean-artifact --check-mode
```
Runs `cargo check` with trace logging to see which artifacts are actually used.
- Faster than build mode (~10-20 seconds)
- More accurate than default mode
- May miss some `.rlib` files that are only needed during full builds
- By default uses current feature configuration (auto-detected)
- **Shows cargo compilation progress in real-time**

**3. Build Mode (Most thorough, ~95-99% accurate)**
```bash
cargo clean-artifact --build-mode
```
Runs `cargo build` with trace logging for maximum accuracy.
- Takes longer (~30-60 seconds)
- Most complete detection of unused artifacts
- Recommended for thorough cleanup
- By default uses current feature configuration (auto-detected)
- **Shows cargo compilation progress in real-time**

#### Additional Options

- `--dry-run`: Show what would be removed without actually removing (default behavior)
- `-y, --yes`: Actually remove the files (skip confirmation)
- `--profile <PROFILE>`: Specify build profile(s) to check (default: debug). Can be used multiple times.
- `--all-features`: Enable all features when tracing (thorough but may be slower)
- `--no-default-features`: Disable default features when tracing
- `--features <FEATURES>`: Comma-separated list of features to enable when tracing
- `--allow-shared-target-dir`: Allow cleaning when CARGO_TARGET_DIR is set (use with caution)

#### Examples

```bash
# See what would be removed (dry run with auto-detected features)
ddt clean --check-mode .

# Actually remove unused artifacts
ddt clean --check-mode -y .

# Thorough cleanup with all features
ddt clean --build-mode --all-features -y .

# Clean specific features only
ddt clean --build-mode --features "serde,logging" -y .

# Clean multiple profiles
ddt clean --build-mode --profile debug --profile release -y .
```

#### How It Works

If you run `ddt clean .` from a cargo project using git:

### How It Works

**Default mode**: Uses `.d` dependency files. If an artifact for a specific version exists but it's not in the dependency graph anymore, it will be removed. Currently only removes large files like `.rlib` and `.rmeta`.

**Trace modes** (recommended): Runs cargo with `CARGO_LOG=cargo::core::compiler::fingerprint=trace` to capture exactly which artifacts are referenced during the build process:
- **Check mode**: Uses `cargo check` (~4s, fast, ~90% accurate)
- **Build mode**: Uses `cargo build` (~30s, thorough, ~99% accurate)

Both trace modes properly handle:
- `.rlib` and `.rmeta` file pairing (keeps both if either is used to prevent recompilation)
- Multiple build profiles (debug, release, custom)
- Feature-specific dependencies via `--features`, `--all-features`, `--no-default-features`
- Cross-crate dependencies

**Feature detection**: By default, the tool uses your project's current feature configuration (auto-detected). Use `--all-features` for thorough checking of all dependencies, or `--features` to specify exactly which features to trace.

## Features

- **Trace-based cleaning**: Most accurate artifact detection using cargo's own build trace
- **Real-time progress**: See what's being compiled during trace mode
- **Interactive profile selection**: Automatically detect and let you choose which profiles to clean
- **Feature-aware**: Supports `--all-features`, `--no-default-features`, and `--features`
- **Safe**: Uses `.rlib`/`.rmeta` pairing to prevent false positives and unnecessary recompilation
- **Beautiful output**: Shows top 10 largest files, per-profile breakdowns, and clear summaries

## License

MIT

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

### Options

- `-y, --yes`: Actually remove the files (skip confirmation)
- `-v, --verbose`: Enable verbose output (shows debug logging including target directory and exact command being run). **Note:** This overrides any RUST_LOG environment variable.
- `--check-mode`: Use `cargo check` for tracing (fast)
- `--build-mode`: Use `cargo build` for tracing (thorough)
- `--command <COMMAND>`: Custom build command to trace (overrides --check-mode and --build-mode)
- `--profile <PROFILE>`: Specify build profile(s) to check. Can be used multiple times. If not specified, shows an interactive selector of available profiles.
- `--all-features`: Enable all features when tracing
- `--no-default-features`: Disable default features when tracing
- `--features <FEATURES>`: Comma-separated list of features to enable when tracing
- `--allow-shared-target-dir`: Allow cleaning when CARGO_TARGET_DIR is set (use with caution)

### Examples

```bash
# Interactive profile selection (default when no --profile specified)
cargo clean-artifact --check-mode

# Actually remove with current features
cargo clean-artifact --check-mode -y

# Verbose mode (shows target directory and exact command)
cargo clean-artifact --build-mode --verbose

# Thorough cleanup with all features
cargo clean-artifact --build-mode --all-features -y

# Clean specific features only
cargo clean-artifact --build-mode --features "serde,logging" -y

# Clean specific profiles (skips interactive selector)
cargo clean-artifact --build-mode --profile debug --profile release -y

# Custom build command (full control)
cargo clean-artifact --command "cargo build --release --target x86_64-unknown-linux-gnu" --profile release -y

# Cross-compilation cleanup
cargo clean-artifact --command "cargo build --target aarch64-unknown-linux-gnu" --profile debug -y
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

**Feature detection**: By default, the tool **auto-detects** which features were used in previous builds by parsing fingerprint files in `target/{profile}/.fingerprint/`. It then **validates** these against your current `Cargo.toml` to filter out any outdated features. This ensures the trace uses the same features you've been building with, making it faster and more accurate than `--all-features`.

Example output:

```text
🔎 Auto-detected features: rayon, p3p, kornia-pnp
⚠️  Ignoring outdated features: tracing-subscriber, old-feature
🔍 Tracing dependencies using cargo Build with features: rayon,p3p,kornia-pnp...
```

If you want to override auto-detection:

- `--all-features`: Check all possible features (thorough but slower)
- `--features "a,b,c"`: Check specific features only
- `--no-default-features`: Check without default features

**Profile detection**: When no `--profile` is specified in trace modes, the tool scans your target/ directory and shows an interactive selector for available profiles (debug, release, custom, etc.). You can select multiple profiles using Space and confirm with Enter.

## Features

- **Trace-based cleaning**: Most accurate artifact detection using cargo's own build trace
- **Real-time progress**: See what's being compiled during trace mode
- **Smart feature auto-detection**: Parses fingerprint files to detect which features were used in previous builds
- **Interactive profile selection**: Automatically detect and let you choose which profiles to clean
- **Custom build commands**: Full control with `--command` flag for complex build scenarios
- **Feature-aware**: Supports `--all-features`, `--no-default-features`, and `--features`
- **Safe**: Uses `.rlib`/`.rmeta` pairing to prevent false positives and unnecessary recompilation
- **Beautiful output**: Shows top 10 largest files, per-profile breakdowns, and clear summaries

## License

MIT

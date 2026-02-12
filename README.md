# ddt

Dudy dev tools.

# Installation

```sh
cargo install ddt
```

## `ddt git`

### `ddt git resolve-lockfile-conflict`

This command allows you to resolve conflicts in lockfiles automatically.

#### Usage

Credit: https://github.com/Praqma/git-merge-driver#documentation

Add a custom merge driver to your **global** gitconfig file. (Typically `~/.gitconfig`)

```gitconfig
[merge "ddt-lockfile"]
	name = A custom merge driver used to resolve conflicts in lockfiles automatically
	driver = ddt git resolve-lockfile-conflict  %O %A %B %L %P

```

then, add some entries to the `.gitattributes` of your project.
You can specify this multiple times.

If your project uses `pnpm` and `cargo` for managing dependencies, you can add this to `.gitattributes`:

```gitattributes
 pnpm.yaml merge=ddt-lockfile
 Cargo.lock merge=ddt-lockfile
```

## `ddt clean`

### Features

- Clean dead git branches.
- Remove **outdated** cargo artifacts using three different modes.

---

Usage: `ddt clean path/to/dir [OPTIONS]`

#### Cleaning Modes

**1. Default Mode (Fast, ~50-60% accurate)**
```bash
ddt clean .
```
Uses `.d` dependency files to detect unused artifacts. Fastest but may miss some stray files.

**2. Check Mode (Recommended, ~80-90% accurate)**
```bash
ddt clean --check-mode .
```
Runs `cargo check` with trace logging to see which artifacts are actually used.
- Faster than build mode (~10-20 seconds)
- More accurate than default mode
- May miss some `.rlib` files that are only needed during full builds
- By default uses current feature configuration (auto-detected)

**3. Build Mode (Most thorough, ~95-99% accurate)**
```bash
ddt clean --build-mode .
```
Runs `cargo build` with trace logging for maximum accuracy.
- Takes longer (~30-60 seconds)
- Most complete detection of unused artifacts
- Recommended for thorough cleanup
- By default uses current feature configuration (auto-detected)

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

#### Dead Git Branches

- dead git branches if you pass `--remove-dead-git-branches`

The dead branch is determined by running `git fetch --all`, and branches are removed if upstream tracking branch is gone.

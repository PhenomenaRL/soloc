# Contributing to soloc

Thank you for your interest in contributing. This document covers development setup, conventions, and the pull request process.

## Prerequisites

- **Rust stable ≥ 1.85** — `spacetimestamp` and `soloc` use the 2024 edition. Install via [rustup](https://rustup.rs/).
- **NAIF ephemeris kernels** (optional) — required only for astronomical frame transforms in tests and the server. Set `SOLOC_KERNEL_PATHS` to a colon-separated list of `.bsp`/`.pca` files, or omit it to let the server download them on first run.

## Building

```bash
# Build the three public crates
cargo build -p spacetimestamp -p soloc -p soloc-server

# Release build of the server binary
cargo build --release -p soloc-server
```

## Testing

```bash
# Run all tests across the three public crates
cargo test -p spacetimestamp -p soloc -p soloc-server

# Run a specific test by name
cargo test -p spacetimestamp test_frame_registry_validation

# Run benchmarks
cargo bench -p spacetimestamp
cargo bench -p soloc
```

One test (`test_celestial_snapshot_real_almanac`) is marked `#[ignore]` because it requires a ~150 MB ephemeris download. Run it explicitly when needed:

```bash
cargo test -p soloc test_celestial_snapshot_real_almanac -- --ignored
```

## Code Conventions

- **Comments**: add a comment only when the *why* is non-obvious — a hidden constraint, a subtle invariant, a workaround for a specific bug. If removing the comment wouldn't confuse a future reader, skip it.
- **Scope**: don't add features, refactor, or introduce abstractions beyond what the task requires. Three similar lines is better than a premature abstraction.
- **Error handling**: validate at system boundaries (user input, external APIs). Don't add fallbacks for scenarios that can't happen inside the codebase.
- **Formatting**: `cargo fmt` is enforced by CI. Run it before pushing.
- **Clippy**: `cargo clippy -p spacetimestamp -p soloc -p soloc-server -- -D warnings` must pass.

## Commit Style

Follow the conventions already established in the git log:

- Imperative mood, ≤ 72-character subject line
- `add` for new functionality, `fix` for bug fixes, `update` for enhancements, `remove` for deletions
- No ticket reference required in the message body, but appreciated

## Submitting Issues

Use the [issue templates](.github/ISSUE_TEMPLATE/) — blank issues are disabled. Please include:

- Your Rust version (`rustc --version`)
- Your OS
- The crate(s) involved
- A minimal reproduction for bug reports

## Submitting Pull Requests

1. Branch off `main`: `git checkout -b my-feature`
2. Keep PRs focused — one logical change per PR
3. Ensure the following pass locally before opening the PR:
   ```bash
   cargo test -p spacetimestamp -p soloc -p soloc-server
   cargo clippy -p spacetimestamp -p soloc -p soloc-server -- -D warnings
   cargo fmt --all -- --check
   ```
4. Fill out the [PR template](.github/PULL_REQUEST_TEMPLATE.md)
5. A maintainer will review and merge

By submitting a pull request you agree to license your contribution under the terms of both [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE).

# Contributing to soloc

Thanks for your interest! soloc is in early development — APIs are unstable and design
input is as valuable as code.

## Getting started

**Prerequisites:** Rust ≥ 1.88 (edition 2024 + let-chains).

```bash
cargo build --workspace
cargo test --workspace
```

The default test suite downloads nothing. Tests that need real SPICE ephemeris data
(~150 MB on first run, cached afterwards) are `#[ignore]`d; run them with:

```bash
cargo test --workspace -- --ignored
```

To avoid the download entirely, point `SOLOC_KERNEL_PATHS` at local kernel files
(e.g. `de440s.bsp`, `pck11.pca`).

## Making changes

1. **Open an issue first for anything non-trivial** — especially schema, API, or
   physics-convention changes. It saves you from building something that can't merge.
2. Branch from `main`, make your change, add tests.
3. Before pushing, run what CI runs:

   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

4. Open a PR against `main`.

## Questions

Open a [Discussion](https://github.com/PhenomenaRL/soloc/discussions) rather than an
issue for questions and design ideas.

# Contributing

Thanks for contributing to `rlean`.

## Development Setup

1. Install the Rust toolchain.
2. Install Python 3.10+ for the PyO3-backed crates and Python strategy tests.
3. Clone the repository and work from a feature branch.

## Local Checks

Run the same checks locally that CI enforces:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --message-format short
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

## Pull Requests

- Keep changes focused and include tests for behavior changes.
- Update `README.md` and other docs when the user-facing workflow changes.
- Do not merge with failing `format`, `check`, `clippy`, or `test` status checks.

## Reporting Issues

Open a GitHub issue with a minimal reproduction, expected behavior, and actual behavior when possible.

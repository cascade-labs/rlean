# Development tasks for rlean.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Show the available recipes.
default:
    @just --list

# Format the complete Rust workspace.
format:
    cargo fmt --all

# Run the same formatting, check, and clippy gates used on pull requests.
lint:
    cargo fmt --all --check
    cargo check --workspace --all-targets --message-format short
    cargo clippy --workspace --all-targets -- -D warnings

# Run all workspace tests.
test:
    cargo test --workspace --all-targets

# Run every pull-request gate locally.
ci: lint test

# Install the host CLI binary locally.
install:
    cargo install --path crates/rlean --locked --force

#!/usr/bin/env bash
# Publish every workspace crate to crates.io in dependency (topological) order.
#
# Cargo resolves a crate's dependencies from the live registry at publish time, so
# a crate can only be published after every workspace crate it depends on is already
# on crates.io. This script publishes in that order and waits for each crate to
# become resolvable before moving on.
#
# Usage:
#   scripts/publish-crates.sh              # real publish (needs CARGO_REGISTRY_TOKEN)
#   scripts/publish-crates.sh --dry-run    # dry-run only; no upload
#
# In --dry-run mode only the crates with no internal path dependencies
# (lean-core, lean-plugin) can be fully verified, because cargo cannot resolve an
# unpublished workspace sibling from the registry during a dry run. Those two are a
# meaningful smoke test that packaging + metadata are valid end to end.
set -euo pipefail

DRY_RUN=0
if [ "${1:-}" = "--dry-run" ]; then
  DRY_RUN=1
fi

cd "$(dirname "$0")/.."

# Dependency-ordered publish list. Regenerate with:
#   cargo metadata --format-version 1 --no-deps | scripts/publish-order.py
ORDER=(
  lean-core
  lean-plugin
  lean-data
  lean-scheduling
  lean-statistics
  lean-consolidators
  lean-indicators
  lean-optimization
  lean-orders
  lean-storage
  lean-universe
  lean-alpha
  lean-crypto
  lean-data-providers
  lean-execution
  lean-forex
  lean-futures
  lean-options
  lean-portfolio-construction
  lean-risk
  lean-algorithm
  lean-brokerages
  lean-sdk
  lean-live
  lean-engine
  lean-python-runtime
  rlean
)

# Crates that have no internal (workspace) path dependencies and can therefore be
# fully dry-run-verified against the registry on their own.
LEAF_CRATES=" lean-core lean-plugin "

version() {
  awk '
    /^\[workspace\.package\]/ {inpkg=1; next}
    /^\[/ {inpkg=0}
    inpkg && /^version[[:space:]]*=/ { gsub(/[",]/, "", $0); print $NF; exit }
  ' Cargo.toml
}
VERSION="$(version)"
echo "Workspace version: ${VERSION}"

# Wait until <crate>@<version> is resolvable from the crates.io index (post-publish
# propagation can take a few seconds).
wait_for_crate() {
  local crate="$1" tries=0
  echo "  waiting for ${crate}@${VERSION} to appear on the index..."
  until curl -sf "https://crates.io/api/v1/crates/${crate}/${VERSION}" >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 60 ]; then
      echo "  ERROR: ${crate}@${VERSION} did not appear within ~5 min" >&2
      return 1
    fi
    sleep 5
  done
  echo "  ${crate}@${VERSION} is live."
}

for crate in "${ORDER[@]}"; do
  if [ "$DRY_RUN" -eq 1 ]; then
    if [[ "$LEAF_CRATES" == *" $crate "* ]]; then
      echo "== dry-run publish ${crate} =="
      cargo publish --dry-run -p "$crate" --allow-dirty
    else
      echo "== skip ${crate} in dry-run (depends on unpublished workspace crates) =="
    fi
    continue
  fi

  # Already published at this version? Skip idempotently (lets a failed run resume).
  if curl -sf "https://crates.io/api/v1/crates/${crate}/${VERSION}" >/dev/null 2>&1; then
    echo "== ${crate}@${VERSION} already published — skipping =="
    continue
  fi

  echo "== publishing ${crate}@${VERSION} =="
  cargo publish -p "$crate"
  wait_for_crate "$crate"
done

echo "Done."

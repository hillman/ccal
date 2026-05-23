#!/usr/bin/env bash
# P1 conformance orchestrator. Builds + runs ccal-server on a throwaway data
# dir, drives it with the JS schema.ts helpers (web/test/conformance.ts), then
# asserts the resulting doc is Rust-readable (tests/conformance.rs). Exits
# non-zero on any failure. See docs/plans/web-interface.md.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

DATADIR="$(mktemp -d /tmp/ccal-conf.XXXXXX)"
DOCID="ccal"
export CCAL_SYNC_TOKEN="conf-token"
export CCAL_SYNC_ADDR="127.0.0.1:8801"
export CCAL_SYNC_DATA="$DATADIR"
SERVER_PID=""

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$DATADIR"
}
trap cleanup EXIT

echo "conformance: building + starting ccal-server…"
cargo build --quiet --bin ccal-server
cargo run --quiet --bin ccal-server &
SERVER_PID=$!
for _ in $(seq 1 50); do
  if (exec 3<>/dev/tcp/127.0.0.1/8801) 2>/dev/null; then exec 3>&- 3<&-; break; fi
  sleep 0.2
done

echo "conformance: running JS driver…"
CCAL_CONF_SERVER="ws://127.0.0.1:8801" CCAL_CONF_DOC="$DOCID" \
  node --experimental-strip-types "$REPO/web/test/conformance.ts"

echo "conformance: running Rust readback…"
CCAL_CONFORMANCE_DOC_PATH="$DATADIR/$DOCID.automerge" \
  cargo test --test conformance -- --nocapture

echo "conformance: ALL GREEN."

#!/usr/bin/env bash
# Boots the pinned CortexDB server (integration/cortexdb/) and runs the
# `cortexdb` engine's live tests (contract, office documents, agent
# lifecycle) against it, then tears it down.
#
#   ./scripts/cortexdb-live.sh            # boot, test, tear down
#   KEEP=1 ./scripts/cortexdb-live.sh     # leave the server running after
#   CORTEXDB_VERSION=v0.10.4 ./scripts/cortexdb-live.sh  # explicit version override
#   CORTEXDB_IMAGE=registry/cortexdb@sha256:... ./scripts/cortexdb-live.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# Keep the version convenience for this script while the default stays tied to
# the verified immutable multi-architecture image digest. An explicit image
# override takes precedence.
if [ -z "${CORTEXDB_IMAGE:-}" ] && [ -n "${CORTEXDB_VERSION:-}" ]; then
  export CORTEXDB_IMAGE="cortexdb/cortexdb:${CORTEXDB_VERSION}"
fi
compose=(docker compose --project-name tinymemory-cortexdb-test -f "$root/integration/cortexdb/docker-compose.yml")
port="${CORTEXDB_PORT:-3142}"
# Compose reads the published port from the environment.
export CORTEXDB_PORT="$port"
url="http://127.0.0.1:$port"

# A test run owns its own Compose project and tears it down with its volumes,
# so it must never share one with a server someone is using. Refuse a port
# that already answers rather than reuse or replace what is there.
if curl --silent --max-time 2 "$url/v1/admin/health" >/dev/null 2>&1; then
  echo "something already serves $url; pick a free CORTEXDB_PORT" >&2
  exit 1
fi

cleanup() {
  result=$?
  if [ "$result" -ne 0 ]; then
    "${compose[@]}" logs cortex mock-inference | tail -80 || true
  fi
  if [ -z "${KEEP:-}" ]; then
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  exit "$result"
}
trap cleanup EXIT

"${compose[@]}" up -d --build --wait mock-inference >/dev/null
"${compose[@]}" up -d cortex >/dev/null
for _ in $(seq 1 120); do
  if curl --fail --silent "$url/v1/admin/ready" >/dev/null; then
    break
  fi
  sleep 1
done
curl --fail --silent "$url/v1/admin/ready" >/dev/null || {
  echo "CortexDB did not become ready at $url" >&2
  exit 1
}
echo "CortexDB $(curl --silent "$url/v1/admin/health") at $url"

TINYMEMORY_LIVE_CORTEXDB_URL="$url" cargo test -p tinymemory-integrations --test live_cortexdb -- --nocapture
TINYMEMORY_LIVE_CORTEXDB_URL="$url" cargo test -p tinymemory-integrations --features documents-office --test office_live -- --nocapture
# These tests mutate scopes on one live server. Running them in parallel races
# CortexDB's shared authorization/derivation state and can return 409 while a
# belief build is in flight; preserve the test assertions and serialize only
# this live target.
TINYMEMORY_LIVE_CORTEXDB_URL="$url" RUST_TEST_THREADS=1 cargo test -p tinymemory-integrations --features brain --test live_cortex_lifecycle -- --nocapture

#!/usr/bin/env bash
# Run the full CortexDB simulation through this host's local Ladder.

set -euo pipefail

if [ "$#" -ne 1 ] || [ "$1" != "--ladder" ]; then
  echo "usage: scripts/cortexdb-simulation.sh --ladder" >&2
  exit 2
fi
if [ -z "${LADDER_API_KEY:-}" ]; then
  echo "LADDER_API_KEY must be exported" >&2
  exit 2
fi

dimension="$(
  curl --fail --silent \
    -H "Authorization: Bearer $LADDER_API_KEY" \
    -H "Content-Type: application/json" \
    -d '{"model":"vectors","input":"tinymemory cortex probe"}' \
    http://127.0.0.1:6969/v1/embeddings \
    | jq -r '.data[0].embedding | length'
)"
if [ "$dimension" != "3072" ]; then
  echo "the vectors ladder returned $dimension dimensions; CortexDB requires 3072" >&2
  exit 1
fi

export CORTEX_INFERENCE_URL=http://host.docker.internal:6969/v1
export CORTEX_INFERENCE_KEY="$LADDER_API_KEY"

cleanup() {
  result=$?
  if [ "$result" -ne 0 ]; then
    docker compose --project-name tinymemory-cortex-ladder \
      -f integration/remote-engines/docker-compose.yml \
      --profile cortex logs cortex || true
  fi
  docker compose --project-name tinymemory-cortex-ladder \
    -f integration/remote-engines/docker-compose.yml \
    --profile cortex down --volumes --remove-orphans || true
  exit "$result"
}
trap cleanup EXIT

simulation_id="ladder-$(date +%s)-$$"
cortex_key="${TINYMEMORY_TEST_CORTEX_KEY:-tinymemory-cortex-test}"
export TINYMEMORY_CORTEX_SIMULATION_ID="$simulation_id"

docker compose --project-name tinymemory-cortex-ladder \
  -f integration/remote-engines/docker-compose.yml \
  --profile cortex up -d cortex

for _ in $(seq 1 120); do
  if curl --fail --silent http://127.0.0.1:3141/v1/admin/ready >/dev/null; then
    logs="$(
      docker compose --project-name tinymemory-cortex-ladder \
        -f integration/remote-engines/docker-compose.yml \
        --profile cortex logs cortex
    )"
    if printf '%s\n' "$logs" \
      | grep -Eq 'failed to load cortex.toml|enrichment OFF|auto-layer scheduler OFF'; then
      echo "CortexDB started with a disabled or rejected full-memory configuration" >&2
      exit 1
    fi
    if ! printf '%s\n' "$logs" | grep -q 'provider.*openai-http:vectors:3072'; then
      echo "CortexDB did not pin the configured vectors ladder" >&2
      exit 1
    fi

    cargo run -p tinymemory-remote --example cortex_simulation -- \
      http://127.0.0.1:3141 "$cortex_key"

    before="$(
      curl --fail --silent \
        -H "Authorization: Bearer $cortex_key" \
        'http://127.0.0.1:3141/v1/scopes/list?limit=10000' \
        | jq '.items | length'
    )"
    docker compose --project-name tinymemory-cortex-ladder \
      -f integration/remote-engines/docker-compose.yml \
      --profile cortex restart cortex >/dev/null
    for _ in $(seq 1 120); do
      if curl --fail --silent http://127.0.0.1:3141/v1/admin/ready >/dev/null; then
        after="$(
          curl --fail --silent \
            -H "Authorization: Bearer $cortex_key" \
            'http://127.0.0.1:3141/v1/scopes/list?limit=10000' \
            | jq '.items | length'
        )"
        if [ "$before" -le 0 ] || [ "$after" -lt "$before" ]; then
          echo "CortexDB did not preserve simulation scopes across restart" >&2
          exit 1
        fi
        jq -n \
          --arg scope "tm:simulation/tm:$simulation_id/tm:conversation" \
          '{scope: $scope, query: "Project Aurora launches on Thursday."}' \
          | curl --fail --silent \
              -H "Authorization: Bearer $cortex_key" \
              -H 'Content-Type: application/json' \
              --data-binary @- \
              http://127.0.0.1:3141/v1/recall \
          | jq -e '.layers.events | map(.content.text // "") | any(contains("Project Aurora launches on Thursday."))' \
          >/dev/null
        exit 0
      fi
      sleep 1
    done
    echo "CortexDB did not become ready after restart" >&2
    exit 1
  fi
  sleep 2
done

echo "CortexDB did not become ready within 240 seconds." >&2
exit 1

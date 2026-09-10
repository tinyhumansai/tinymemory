# Remote engine conformance

This harness boots the native self-hosted APIs that `tinymemory-remote` targets.
The Mem0 and Cognee build contexts are pinned to the upstream revisions used
when the dialects were verified. Supermemory's current self-hosted distribution
is its official `supermemory local` server rather than an upstream Compose
file, so the small Dockerfile containerizes that command.

Run one profile at a time from the repository root:

```sh
docker compose -f integration/remote-engines/docker-compose.yml --profile supermemory up -d --build
docker compose -f integration/remote-engines/docker-compose.yml logs supermemory
# Copy the `sm_...` API key printed on first boot.
cargo run -p tinymemory-remote --example conformance -- \
  supermemory http://localhost:6767 sm_...

docker compose -f integration/remote-engines/docker-compose.yml \
  --profile mem0 up -d --build
cargo run -p tinymemory-remote --example conformance -- mem0 http://localhost:8888

docker compose -f integration/remote-engines/docker-compose.yml \
  --profile cognee up -d --build
cargo run -p tinymemory-remote --example conformance -- cognee http://localhost:8001

docker compose -f integration/remote-engines/docker-compose.yml \
  --profile agentmemory up -d --build
cargo run -p tinymemory-remote --example conformance -- agentmemory http://localhost:3111

./scripts/ci/cortexdb-e2e.sh

# Real local Ladder on 127.0.0.1:6969. LADDER_API_KEY must already be exported.
./scripts/cortexdb-simulation.sh --ladder
```

The same conformance command can target managed services. Supermemory uses the
same bearer authentication in both modes, while Cognee Cloud uses its distinct
API-key header:

```sh
cargo run -p tinymemory-remote --example conformance -- \
  supermemory https://api.supermemory.ai "$SUPERMEMORY_API_KEY"

cargo run -p tinymemory-remote --example conformance -- \
  cognee-api "https://tenant-<uuid>.aws.cognee.ai" "$COGNEE_API_KEY"
```

Cognee Cloud has **no shared endpoint** — `api.cognee.ai` resolves in DNS but
nothing listens there (see the constructor note in
`crates/tinymemory-remote/src/cognee.rs`), which is why this crate exports no default
Cognee endpoint constant. The tenant URL printed beside your API key on the
Cognee dashboard is the only address that exists; substitute it above. The command writes a unique conformance namespace,
verifies Core, Recall, and Portability, and deletes its test record before
exiting.

Mem0 and Cognee require an inference provider for their native semantic
pipelines. By default the harness starts a deterministic OpenAI-compatible test
service, which proves HTTP, persistence, embeddings, and adapter translation
without an external credential. Set `OPENAI_API_KEY` and `OPENAI_BASE_URL` to
exercise a real compatible provider instead. The test service is a wiring
fixture, not a quality benchmark.

AgentMemory is pinned to its `v0.9.29` source release and the compatible
`iiidev/iii:0.11.2` engine. Its harness is deliberately zero-LLM: it verifies
the native REST routes, persistence, and TinyMemory envelope translation
without requiring external credentials. CI runs the same harness through
`scripts/ci/agentmemory-e2e.sh` on every push and pull request.

CortexDB is pinned to `v0.9.9`. Its CI profile enables the full Ladder-compatible
memory pipeline against the deterministic OpenAI-compatible fixture. The live
script points the same image and configuration at the host's `vectors`,
`flash`, `reasoning`, and `max-reasoning` ladders. Cohere reranking and binary
media processors are intentionally outside this profile.

The live script fixes the inference destination to `host.docker.internal:6969`;
it cannot be redirected to a remote plaintext host. The Ladder bearer crosses
only the host-local Docker bridge and is never printed or persisted in the
repository. CortexDB's own reusable bearer is separately restricted to HTTPS,
with literal loopback HTTP allowed for this harness.

Stop the harness without deleting its named volumes:

```sh
docker compose -f integration/remote-engines/docker-compose.yml down
```

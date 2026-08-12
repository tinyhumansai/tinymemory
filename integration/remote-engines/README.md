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
```

Mem0 and Cognee require an inference provider for their native semantic
pipelines. By default the harness starts a deterministic OpenAI-compatible test
service, which proves HTTP, persistence, embeddings, and adapter translation
without an external credential. Set `OPENAI_API_KEY` and `OPENAI_BASE_URL` to
exercise a real compatible provider instead. The test service is a wiring
fixture, not a quality benchmark.

Stop the harness without deleting its named volumes:

```sh
docker compose -f integration/remote-engines/docker-compose.yml down
```

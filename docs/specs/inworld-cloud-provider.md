# Inworld cloud provider metadata

**Status:** Accepted

**Owner:** TinyMemory maintainers

## Problem

Hosts that consume TinyMemory's cloud-provider contract cannot represent an
OpenAI-compatible provider that authenticates with HTTP Basic credentials.
They also need one canonical Inworld preset so configuration, migration, and
backend-host safety logic agree on its endpoint and authentication style.

## Goals

- Represent HTTP Basic authentication as a stable `basic` wire value.
- Publish Inworld as a built-in OpenAI-compatible cloud provider.
- Classify Inworld's API host as an inference host and its LLM endpoint as
  chat-completions-only.
- Preserve existing provider metadata and legacy migrations.

## Non-goals

- Store or transmit credentials.
- Implement Inworld model discovery or inference requests.
- Add Inworld voice-provider metadata.

## Proposed behavior

`AuthStyle::Basic` serializes as `"basic"` and reports `"basic"` from
`AuthStyle::as_str()`. The built-in catalog contains:

```text
slug: inworld
label: Inworld
endpoint: https://api.inworld.ai/v1
auth_style: basic
```

Legacy entries with `type: "inworld"` inherit those values when their modern
fields are absent. `api.inworld.ai` is treated as a built-in inference host and
the endpoint remains chat-completions-only.

## Invariants and constraints

- The catalog is the single source for built-in host classification.
- Only OpenAI's first-party preset advertises the Responses API.
- Existing auth-style wire values remain unchanged.
- No credential value is stored in this crate's provider metadata.

## Acceptance criteria

- All auth styles round-trip through JSON with stable lowercase values.
- Catalog and legacy-migration tests produce the documented Inworld preset.
- Host classification recognizes `api.inworld.ai` and disables the Responses
  API fallback for its endpoint.
- The workspace's four contract commands pass.

## Open questions

None.

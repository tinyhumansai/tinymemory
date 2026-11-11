# Repository Guidelines

This file is the single source of truth for how humans and coding agents work
in this repository. `CLAUDE.md` is a symlink to this file, so every agent reads
the same instructions.

Adapt the project-specific parts as the crate changes, and delete guidance that
no longer applies rather than leaving it to rot.

## Project Structure

This is a Cargo **workspace** with a virtual root: there is no root package,
and every crate lives in its own directory under `crates/`, named for the
package it holds. The workspace has four packages: three in the memory layer
and the standalone redaction library `tinyredact`:

- `crates/tinymemory-api` — the **core contract**: `MemoryEngine`, items,
  metadata, namespaces, errors, and (feature `conformance`) the suite every
  engine must pass. No I/O.
- `crates/tinymemory-tools` — the **agent tool spec**: `MemoryTools` over any
  engine, holistic recall and the `context.md` compiler, and the agent memory
  lifecycle (`AgentMemory`, `Brain`, `MemoryLayout`, background jobs).
- `crates/tinymemory-integrations` — the **integrations**: the CortexDB engine
  and its registry, documents, sources, safety and the legacy v1 import, each
  a module behind a feature.
- `crates/tinyredact` — **local redaction**: secret and personal identifier
  detection and scrubbing, with no network access.

Shared package metadata, the version and the lint table live in the root
`Cargo.toml` (`[workspace.package]`, `[workspace.lints]`). The accepted
behaviour is [`docs/specs/memory-v2.md`](docs/specs/memory-v2.md); how it is
built is in [`docs/architecture/`](docs/architecture/README.md).

See [`README.md`](README.md) for the full layout and the feature table.

```text
crates/<package>/
├── Cargo.toml          # one package; `[lints] workspace = true`
├── README.md           # required of complex crates: design, surface, caveats
└── src/
    ├── lib.rs          # crate docs + the entire public re-export surface
    ├── error/mod.rs    # crate-wide `Error` and `Result<T>`
    └── <feature>/      # one directory per feature area
        ├── mod.rs      # module docs, wiring, smallest useful public API
        ├── types.rs    # substantial type definitions
        └── mod_tests.rs # module-local unit tests
crates/<package>/tests/     # integration tests against the public API only
crates/<package>/examples/  # runnable, compiled-in-CI usage examples
docs/
├── specs/              # behavior and architecture specifications
├── plans/              # test-first implementation plans
└── adr/                # immutable architecture decision records
```

Do not add another crate without an explicit architecture decision. New
contract surface goes in `tinymemory-api`, a new agent-facing tool in
`tinymemory-tools`, and anything that talks to the outside world (a new engine,
reader or converter) becomes a module of `tinymemory-integrations` behind a
feature named after it, with its dependencies optional and enabled only by that
feature. Keep standalone redaction changes in `tinyredact`.

Each feature area belongs in a focused module directory under the crate's
`src/`. A module root explains the module, wires its pieces together, and
exposes the smallest useful API. Move substantial type definitions into
`types.rs` and put module-local unit tests in a sibling `<module>_tests.rs`, wired from
the bottom of the module root with:

```rust
#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
```

Do not write inline `mod tests` blocks in implementation files, do not name a test
file `test.rs`, `tests.rs` or `<module>_test.rs`, and do not
let a general-purpose `utils.rs` or `helpers.rs` grow — those are a symptom of a
missing module. Prefer many small modules that each do one thing well over few
broad ones.

Keep public exports centralized in each crate's `src/lib.rs` so downstream
users have one predictable surface. Put shared error variants in
`src/error/mod.rs` and return the crate-wide `Result<T>` from fallible public
APIs.

## Build And Test

Run every command from the repository root. These four are the contract; CI
runs exactly them, so a green local run should mean a green CI run.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

Supporting commands:

- `cargo fmt --all` — format before committing.
- `cargo test <filter>` — run a focused subset while iterating.
- `cargo run -p tinymemory-integrations --example basic` — run the bundled example. The
  `-p` is required: the workspace root is virtual, so cargo cannot infer which
  package an example belongs to.
- `cargo doc --no-deps --all-features` — build the rustdoc CI also builds with
  `RUSTDOCFLAGS="-D warnings"`.
- `cargo test --doc` — run doctests alone when editing documentation examples.

Never skip, ignore, or delete a failing test to make a command pass. Fix the
root cause, or stop and report the blocker.

## Coding Style

Use standard `rustfmt` output and Rust 2024 idioms. Do not hand-format around
`rustfmt`, and do not add `#[rustfmt::skip]` without a comment explaining why.

- `snake_case` for modules, files, functions, methods, fields, and locals.
- `PascalCase` for types, traits, and enum variants; `SCREAMING_SNAKE_CASE` for
  constants and statics.
- Name things for what they are, not for their layer: `RetryPolicy`, not
  `RetryHelper`.
- Prefer small, typed APIs over stringly-typed ones. Accept `&str` and generic
  `impl Into<String>` at boundaries; return owned, concrete types.
- Keep the public surface minimal: default to private, and export deliberately
  from the crate's `src/lib.rs`.
- `unsafe` is forbidden in every crate by `[workspace.lints]` in the root
  `Cargo.toml`, which each crate inherits with `[lints] workspace = true`. If a
  crate genuinely needs it, relax the lint in its own commit and document every
  invariant with a `// SAFETY:` comment.

### Errors

- One crate-wide `Error` enum in the crate's `src/error/mod.rs`, built with
  `thiserror`.
- Fallible public functions return `Result<T>`, the crate alias.
- Add a specific variant instead of stuffing context into a string; error
  messages are lowercase, without trailing punctuation.
- Do not `unwrap()`, `expect()`, or `panic!` in library code paths. They are
  fine in tests, examples, and genuinely unreachable states — where `expect`
  must carry a message explaining the invariant.
- Document a `# Errors` section on every public fallible function and a
  `# Panics` section on anything that can panic.

### Dependencies

Adding a dependency is a design decision. Before adding one, check whether the
standard library or an existing dependency already covers the need. When you do
add one:

- pin a caret range (`serde = "1"`), not an exact version;
- enable only the features you need, with `default-features = false` when that
  meaningfully trims the tree;
- gate anything optional behind a Cargo feature, documented in `Cargo.toml`;
- leave a comment above the entry explaining *why* the crate is needed and what
  uses it — see the existing entries for the expected tone;
- prefer well-maintained crates with a compatible license.

Keep `Cargo.lock` committed; this crate ships a lockfile so CI and releases are
reproducible.

## Testing

- Module-local unit tests live in `crates/<package>/src/<feature>/mod_tests.rs` and
  may touch private items.
- Integration tests live in `crates/<package>/tests/` and exercise only the
  public API — they are the regression suite for the crate's contract.
- Use descriptive, behavioral test names: `rejects_an_empty_name`, not
  `test_greet_2`.
- Cover the failure paths, not just the happy path. Every new error variant
  needs a test that produces it.
- For async behavior, standardize on one runtime (`tokio` as a dev-dependency
  for tests) rather than mixing runtimes.
- Tests must be deterministic and independent of network, wall-clock time, and
  execution order. Gate any live/network test behind a feature or an env var and
  name it `live_*` so it is easy to exclude.
- Maintain at least 80% coverage of meaningful library behavior. Add or update
  tests with every behavior change, and note any deliberately untested edge case
  in the pull request description.

Write the test first when fixing a bug: a failing test that reproduces the
report, then the fix that turns it green.

## Documentation

Write documentation for the reader who has never seen the code.

- Every public item gets a rustdoc comment. `missing_docs` is a warning that CI
  treats as an error.
- Start every `mod.rs` and `*_tests.rs` with a concise module-level `//!`
  description.
- Each crate's `src/lib.rs` carries its crate-level overview: what the crate
  does, its primary entry points, and a short runnable example.
- Prefer concrete examples over vague description. Doc examples are compiled and
  run by `cargo test`, so they cannot drift.
- Complex modules must include a module-level `README.md` covering their design,
  public surface, and important operational constraints.
- Keep `README.md`, `docs/`, and module docs aligned with code changes in the
  same commit that changes behavior.
- Write accepted behavior and constraints in `docs/specs/` before creating a
  linked, implementation-ordered plan in `docs/plans/`. Specs define what and
  why; plans define how and in what sequence.
- Keep every Markdown file, including this one, at 500 lines or fewer. When a
  topic outgrows that, split it into focused files and link them from the
  nearest `README.md`.

## Git Workflow

- Never commit directly to `main`. Branch first, one branch per logical change.
- Do feature work in a git worktree so the main checkout stays clean.
- Commit subjects are concise and imperative: `Add retry policy to the client`.
  Keep the subject specific to the change and under ~72 characters.
- Make small, focused commits. Each commit should cover one logical change,
  build independently, and avoid mixing formatting, refactors, and behavior
  changes unless they are inseparable.
- Never commit secrets. `.env` is git-ignored; document new variables in
  `.env.example` with placeholder values.
- Never force-push a shared branch, rewrite published history, or bypass hooks
  with `--no-verify`.

## Pull Requests

Open pull requests ready for review, not as drafts, unless the work genuinely
must not merge yet. A pull request should:

- summarize what changed and why, in a few sentences;
- call out public API or behavior changes explicitly, or state "None";
- list the validation commands actually run, with their outcome;
- link the related issue;
- include updated tests, docs, and examples in the same change.

The template in `.github/PULL_REQUEST_TEMPLATE.md` encodes this checklist.
Address review feedback by fixing it, and reply on each thread describing what
changed. Do not resolve a thread whose feedback you have not addressed or
explicitly declined with a reason.

## Releases

Releases run from `.github/workflows/release.yml` via a manual
`workflow_dispatch` with a `patch` / `minor` / `major` bump. The workflow
re-runs formatting, clippy, tests, and rustdoc, computes the next version,
updates `[workspace.package] version` in the root `Cargo.toml` and
`Cargo.lock`, commits and tags
`vX.Y.Z`, pushes both, and creates a GitHub release for the tag. There are no
binary artifacts.

Nothing is published to crates.io: every crate is `publish = false`, and hosts
take this repository by git or path, pinned to a tag.

Consequently:

- Do not hand-edit `[workspace.package] version` in the root `Cargo.toml`; the
  release workflow owns it.
- Follow semantic versioning. Any change to the public surface that is not
  purely additive is a breaking change and needs a major bump.
- `main` must always be green: a release re-runs the validation suite on it.

## Agent Working Agreement

For automated contributors specifically:

1. **Read before writing.** Inspect the surrounding module and match its
   conventions, comment density, and idiom rather than importing a house style.
2. **Verify, do not assume.** Run the four contract commands and read their
   output before reporting a task complete. Report failures with the output;
   never claim a check passed that you did not run.
3. **Stay in scope.** Implement what was asked. Do not opportunistically
   refactor, reformat, upgrade dependencies, or "fix" unrelated code — raise it
   instead.
4. **No placeholders in delivered code.** No `todo!()`, no stubbed functions, no
   commented-out alternatives left behind. If something cannot be finished, say
   so explicitly.
5. **Do not weaken the guardrails.** Never add blanket `#[allow(...)]`, relax a
   lint, mark a test `#[ignore]`, or loosen CI to get a green run. Fix the
   cause.
6. **Secrets stay out.** Never read, echo, or commit `.env` contents, tokens, or
   credentials, and never paste them into a pull request or issue.
7. **Ask only when blocked.** Make routine judgment calls yourself; escalate
   only irreversible decisions or genuine forks with no clear default.

## Tests live in `*_tests.rs` files

- Unit tests are never inline. Do not write a `#[cfg(test)] mod tests { ... }`
  block in a source file. Put the tests in a sibling `<module>_tests.rs`
  (`mod_tests.rs` beside a `mod.rs`, `lib_tests.rs` beside `lib.rs`) and declare
  it at the bottom of the module:

  ```rust
  #[cfg(test)]
  #[path = "foo_tests.rs"]
  mod tests;
  ```

- The test file starts with `use super::*;` and carries no `#[cfg(test)]` of its
  own. It is still a child module, so it reaches private items exactly as an
  inline module did.
- Name test files `<module>_tests.rs`; a second group for the same module is
  `<module>_<topic>_tests.rs`. Never `test.rs`, `tests.rs` or `<module>_test.rs`.
- Integration tests stay in the crate's `tests/` directory.
- OpenHuman's `scripts/externalize-inline-tests.mjs <repo-root> --write` moves
  inline test modules out mechanically; without `--write` it only reports.

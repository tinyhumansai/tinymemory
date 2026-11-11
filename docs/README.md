# Documentation

This directory holds documentation that does not belong in rustdoc: the shape
of the system, the reasoning behind it, and the constraints a reader needs
before touching the code. API reference lives in doc comments next to the code,
where it cannot drift.

## Layout

```text
docs/
├── README.md      # this index
├── integration.md # how an agent host wires in the memory API
├── architecture/  # how the code is built, one document per concern
├── specs/         # behavior and architecture specifications
├── plans/         # implementation plans derived from approved specs
├── evals/         # accuracy and latency evals, with recorded runs
└── adr/           # architecture decision records, numbered and immutable
```

- **[`integration.md`](integration.md)** — for an agent host (OpenHuman or
  any other): which calls to make around each turn, what they cost, the
  rules that keep recall accurate, and how to bring your own engine.
- **[`architecture/`](architecture/README.md)** — the shape of the memory crates
  and the separate redaction library:
  the core contract, operation semantics, namespaces, the CortexDB engine, the
  tools, the integrations and the test strategy.
- **[`specs/`](specs/README.md)** — one file per feature, module, or subsystem,
  describing its behavior, public surface, invariants, and acceptance criteria.
- **[`plans/`](plans/README.md)** — implementation-ordered, test-first steps for
  delivering an approved specification. Plans name exact files and verification
  commands, and are updated as the work progresses.
- **[`evals/`](evals/README.md)** — how well the memory serves an agent,
  measured: each eval's method, how to rerun it, and the results of recorded
  runs.
- **`adr/`** — a dated record per significant decision. Use
  [`adr/0001-record-architecture-decisions.md`](adr/0001-record-architecture-decisions.md)
  as the template. An accepted ADR is not edited; it is superseded by a later
  one.

Complex modules also carry a module-level `README.md` inside `src/<module>/`
covering their design, public surface, and important constraints.

## Conventions

- Keep every Markdown file at 500 lines or fewer. When a topic outgrows that,
  split it into focused files and link them from the nearest `README.md`.
- Update documentation in the same commit as the behavior it describes.
- Prefer a concrete example over an abstract description.
- Link between documents rather than duplicating content; one fact lives in one
  place.
- Write a specification before a plan: the spec defines the outcome and
  constraints, while the plan defines the implementation sequence.

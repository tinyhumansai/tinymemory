# CortexDB full integration plan

1. Add CortexDB to the shared driver vocabulary, facade features, registry,
   feature tests, and documentation.
2. Compose the existing Cortex memory adapter into a capability-complete
   `CortexProvider` and implement the five granular operations over native v1
   experience, bulk-experience, recall, and answer routes.
3. Extend adapter and conformance tests for the advertised operations,
   including tool calls through `RawMemoryEvent`.
4. Add the pinned Docker profile, deterministic inference fixture, CI runner,
   and opt-in live-Ladder runner.
5. Run formatting, clippy, build, tests, coverage, feature powerset, rustdoc,
   module validation, and both deterministic and live simulations.

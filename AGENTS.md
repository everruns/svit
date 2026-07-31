## Coding-agent guidance

### Style

Be concise. Prefer explicit domain terms over metaphors: runtime, process,
activation, memory tree, script library, outbox, snapshot, and fork.

### Principles

- Fix root causes. Read more code before guessing.
- Keep changes small, runnable, and testable end to end.
- Write a failing regression test before fixing a bug.
- Important design decisions belong in `knowledge/` and in a short comment at
  the implementation boundary that enforces them.
- Do not preserve a weak interface for compatibility. Svit is research-stage.
- Never describe a control as proven or mitigated without executable evidence.

### Current vertical slice

The first slice is deliberately narrow:

1. One isolated process owns one serializable memory tree and named Lua scripts.
2. One activation runs a named script against a transactional working copy.
3. Success commits memory, scripts, and buffered message intents atomically.
4. Any syntax, runtime, conversion, limit, or validation failure rolls back all
   activation changes.
5. A committed process can be snapshotted, restored, and forked.
6. Guest code has no ambient filesystem, network, environment, process, module
   loader, native extension, clock, or randomness access.

Scheduling, message delivery, projections, durable storage adapters,
distributed identity, and production multi-tenant deployment are not part of
this slice. Record pressure to add them in
`knowledge/operations/limitations.md`; do not add speculative abstractions.

### Security invariants

- Treat every guest script, input value, snapshot, address, and message as
  untrusted.
- Guest-visible references are typed values, never authority-bearing strings.
- Host access requires an explicit typed capability. The initial slice grants
  none.
- Resource limits are semantics, not optional hardening.
- Validate guest values before commit: bounded depth and size, text map keys,
  acyclic collections, and finite floating-point values.
- Keep process state private. Forked roots may share immutable storage, but
  mutations must never cross process boundaries.
- Sanitize and cap guest-visible diagnostics. Do not expose host paths,
  backtraces, pointers, or dependency internals.
- Keep `unsafe` out of the trusted core unless a written security decision and
  focused tests justify it.
- Do not claim hostile same-native-process multi-tenancy is proven safe. The
  embedded interpreter is one layer; production isolation requires additional
  evidence and likely a Wasm or OS boundary.

### Knowledge

`knowledge/` is the canonical OKF v0.2 bundle and persistent project memory.
Read the relevant concepts before changing behavior. Update them in the same
change when an invariant, decision, limitation, threat, test strategy, or
operational fact changes. Follow `knowledge/knowledge-contract.md` and run
`just check-okf`.

| Knowledge | Purpose |
| --- | --- |
| `foundations/architecture.md` | Trusted core and module boundaries |
| `foundations/process-model.md` | Process, activation, commit, snapshot, and fork semantics |
| `runtimes/lua-runtime.md` | Versioned Svit Lua guest contract |
| `security/threat-model.md` | Assets, trust boundaries, stable threat IDs, controls |
| `security/security-testing.md` | Adversarial and invariant test requirements |
| `operations/testing.md` | Test and example organization |
| `operations/limitations.md` | Honest negative specification |

### Documentation

- `README.md`: product orientation and the shortest runnable path.
- `docs/`: public guides and research documents.
- `crates/svit/docs/`: guides embedded in Rustdoc, when needed.
- `knowledge/`: durable engineering decisions, not marketing material.
- `skills/svit/`: public agent-facing usage guidance.

Examples are a first-class API surface. Every example must contain assertions,
produce deterministic output, and run in CI. Prefer examples that demonstrate
memory persistence, self-authored scripts, transaction rollback, snapshot and
fork isolation, and sandbox limits.

### Local development

```bash
just --list
just build
just test
just examples
just check
just pre-pr
```

If a command is temporarily unavailable while the initial workspace is being
assembled, use the equivalent direct Cargo or Python command and update the
`justfile` in the same change that makes the command stable.

Rust requirements:

- Use the pinned stable toolchain in `rust-toolchain.toml`.
- Run `cargo fmt --check`.
- Run `cargo clippy --all-targets --all-features -- -D warnings`.
- Public APIs require Rustdoc and examples.
- Prefer domain-specific error variants over catch-all strings.
- Constructors and private fields must enforce process invariants.

### Tests

Changes to the transition boundary require both positive and rollback tests.
At minimum, test:

- successful activation commits once;
- each failure class preserves version, memory, scripts, and outbox;
- snapshot round trips preserve canonical state and hash;
- replay from the same snapshot and input is deterministic;
- fork mutations do not affect parent or siblings;
- separate processes do not share Lua globals;
- denied libraries and dynamic loading remain unavailable;
- every configured resource limit fails closed.

Unit tests live beside implementation. Cross-module behavior belongs under
`crates/svit/tests/`. Executable Rust demonstrations live under
`crates/svit/examples/`; complete consumer scenarios live under `examples/`.

### Threat model maintenance

Threat IDs use `TM-<CATEGORY>-<NUMBER>` and are never reused. New attack
surfaces require:

1. an entry in `knowledge/security/threat-model.md`;
2. a `THREAT[TM-XXX-NNN]` comment at the mitigation boundary;
3. a regression or invariant test referencing the same ID;
4. a public security-doc update when caller behavior changes.

Leave a threat `REQUIRED` or `OPEN` until the mitigation and its test exist.

### Commits and pull requests

Use Conventional Commits: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`,
`perf`, `build`, or `ci`. Repository guidance and knowledge-only changes use
`chore` or `docs`. Attribute commits only to the human user; never add AI
co-author or generation notices. Each commit that lands on `main` must be a
coherent semantic unit and pass its relevant quality gates.

Pull requests are a coordination tool, not a mandatory delivery step. Use one
only when review, CI policy, or collaboration requires it. Squash-merge pull
requests so `main` receives one semantic commit rather than review history.
Release notes, changelogs, and tag messages must describe changes directly and
must not link to pull requests; release preparation may curate unpublished
history, making PR references unstable. Never merge or ship with failing CI.

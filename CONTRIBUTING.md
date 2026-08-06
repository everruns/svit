# Contributing to Svit

Svit is a research-stage runtime. Contributions should sharpen one executable
property at a time rather than broaden the surface speculatively.

## Setup

Install the pinned Rust toolchain through rustup, then install the local task
runner and security tools:

```console
cargo install just cargo-audit cargo-deny
just build
just test
just examples
```

`just check-okf` also uses `okf-lint` when installed:

```console
cargo install okf-lint --version 0.1.1 --locked
```

## Development workflow

1. Read `AGENTS.md` and the relevant concept under `knowledge/`.
2. Create a focused branch from current `main`.
3. For a bug, add a failing regression test first.
4. Implement the smallest complete vertical change.
5. Add positive, negative, rollback, and limit tests as applicable.
6. Update executable examples and durable knowledge in the same change.
7. Run `just pre-pr` and deliver one semantic commit. Open a pull request only
   when review, repository policy, or collaboration requires one.

## Commands

```console
just --list
just build        # Build the workspace
just test         # Run Rust tests and doctests
just examples     # Execute all acceptance examples and the CLI smoke test
just check-okf    # Validate the knowledge bundle
just check        # Format, clippy, tests, docs, and repository validators
just audit        # cargo-audit and cargo-deny
just pre-pr       # Complete local quality gate
```

## Design and knowledge

`knowledge/` is the canonical OKF v0.2 engineering memory. If a change alters
an invariant, process or language semantics, limitation, threat, or testing
requirement, update the corresponding concept and `knowledge/log.md`.

Keep planned features honest. Scheduling, delivery, projections, capabilities,
distributed identity, production isolation, and formal verification remain
limitations until implementation and evidence land.

## Testing security-sensitive changes

Guest scripts, values, snapshots, addresses, and message bodies are untrusted.
Changes to parsing, conversion, Lisp exposure, limits, transactions, snapshots,
forks, hooks, or errors require review against
`knowledge/security/threat-model.md` and tests following
`knowledge/security/security-testing.md`.

New threats use a stable `TM-<CATEGORY>-<NUMBER>` identifier in the threat
model, mitigation comment, and regression test. Leave their status `REQUIRED`
or `OPEN` until executable evidence exists.

## Style and commits

- Run `cargo fmt` and Clippy with warnings denied.
- Keep public APIs documented and demonstrated.
- Prefer domain types and specific errors over strings and catch-all variants.
- Use Conventional Commits, for example `feat(process): add snapshot restore`.
- Never include secrets, AI attribution, or agent-session links.

## Delivery and releases

Keep each commit on `main` to one coherent, buildable semantic change. Pull
requests are optional: when one is useful, keep it focused, include evidence
and security impact, and squash-merge it into one semantic commit.

Release notes, changelog entries, and annotated tags describe changes without
pull-request numbers or links. Release preparation may curate unpublished
history, so the canonical reference for shipped behavior is the release tag
and its source commit. Do not merge or ship while required checks are failing.

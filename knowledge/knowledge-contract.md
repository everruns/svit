---
type: Playbook
title: Knowledge Maintenance Contract
description: Rules for maintaining Svit's durable engineering knowledge and OKF conformance.
tags:
  - svit
  - knowledge
  - okf
---

# Knowledge Maintenance Contract

`knowledge/` is Svit's canonical Open Knowledge Format (OKF) v0.2 bundle and
persistent engineering memory.

## Maintenance rules

- Read relevant concepts before changing behavior.
- Update knowledge in the same change when code alters a documented decision,
  invariant, limitation, threat, test strategy, or operational process.
- Record decisions that cannot be recovered reliably from code. Link to code
  and tests rather than copying volatile implementation detail.
- Stable identifiers such as `TM-*` and `L-*` are never renumbered or reused.
- Public vision, concepts, and user instructions belong in `docs/`; agent usage
  guidance belongs in `skills/svit/`; internal research, decisions,
  specifications, and operational facts belong here.
- An unimplemented target must say `Under implementation`, `Required`, or
  `Open`. Do not write about planned behavior in the present tense.

## OKF layout

The bundle declares `okf_version: "0.2"` in the root `index.md`.

- Every concept document starts with YAML frontmatter containing non-empty
  `type`, `title`, and one-line `description` fields.
- Reserved `index.md` files have no frontmatter except the bundle root, which
  may contain only `okf_version`.
- Every directory has an `index.md` listing its immediate concepts and
  subdirectories, and nothing deeper.
- `log.md` has no frontmatter and groups updates under newest-first
  `## YYYY-MM-DD` headings.
- Concept links are relative and must resolve.

Current concept types are `Architecture`, `Process Model`, `Language Contract`,
`Threat Model`, `Test Strategy`, `Limitations`, `Research Proposal`, and
`Playbook`.

## Enforcement

Run both the repository validator and the upstream linter when available:

```console
python3 scripts/check_okf.py knowledge
okf-lint knowledge --max-line-length 10000
```

The dependency-free repository validator also runs through:

```console
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
```

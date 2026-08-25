---
name: svit
description: Use when a user wants to run Svit, write Svit Lisp scripts, work with process memory, named scripts, activations, snapshots, forks, or understand Svit's current sandbox and limitations.
---

# Svit

Svit is a research-stage Rust runtime for durable reasoning, transactional
process state, and restricted Lisp scripting.

## How to use this skill

Lead with the smallest runnable example from the repository. Confirm the
current API in `README.md`, `crates/svit/examples/`, and `examples/` before
giving exact method names because the project is pre-release.

Load only the reference needed:

- Core concepts and terminology: `references/concepts.md`
- Example selection: `references/examples.md`
- Security boundary and current gaps: `references/security.md`
- Lisp helper discovery and structured agent scripting: `references/lisp-runtime.md`; call `(runtime-builtins)` in the target process before authoring Lisp.

## Response rules

- Call the runnable unit **Svit**. It owns one reason/act loop, one durable
  conversation thread, and exactly one **process**.
- Call the embedding application the **host** and describe Everruns as Svit's
  current loop implementation.
- Call the host-selected model and provider a **Reasoner**.
- Describe a single script invocation as an **activation**.
- Call the controlled concurrency and commit model **VAST: Versioned Atomic
  State Transitions**. Keep `svit-control@1` as the wire protocol identifier.
- Treat memory, named scripts, and buffered messages as one atomic committed
  process state.
- Use `discover`, `read`, `stat`, `write`, `remove`, and `exec` with absolute
  process paths across Rust, model tools, and Svit Lisp.
- State that guest Lisp has no ambient host filesystem, network, environment,
  clock, randomness, process, module-loader, or native-extension access.
- Do not describe buffered message intents as delivered messages.
- Do not describe a local process address as authenticated identity.
- Do not claim production-grade multi-tenancy or formal proof. Refer to the
  threat model and limitations when security assurances matter.
- Prefer executable examples that demonstrate both success and rollback.

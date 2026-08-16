---
type: Test Strategy
title: Reasoning Scenarios
description: Deterministic model-driven acceptance scenarios for the complete Svit process surface.
tags:
  - svit
  - reasoning
  - testing
---

# Reasoning Scenarios

## Status

The collection starts with one implemented scenario. Add a scenario when a
model-facing workflow exercises a cross-boundary contract that unit tests do
not make legible.

## Rules

Each scenario uses a deterministic Everruns scripted reasoner and deterministic
host fixtures, but must exercise the public Svit surface exactly as a live
model would. It may use `discover`, `read`, `stat`, `write`, `remove`, and
`exec`, named `/lib` scripts, Svit Lisp standard-library functions, and
host-attached `/ports`. It must not preload its expected answer, call a
scenario-specific host helper, or inspect private process state.

A scenario states the observable durable result and is implemented as an
integration test under `crates/svit/tests/`. A live external variant may be
run manually, but is not deterministic CI evidence and must not receive
credentials from pull-request-controlled code.

## SC-001: Summarize a model catalog

**Status:** Implemented in
`crates/svit/tests/reasoning_loop.rs::model_catalog_scenario_uses_a_saved_script_to_reduce_http_into_memory`.

**Task:** Fetch `https://models.dev/models.json`, report the number of models,
and add information about the latest GPT models to memory.

**Required behavior:**

1. The model writes a documented named script at
   `/lib/summarize-model-catalog`.
2. The model invokes that path through `exec`; it does not provide inline
   source to `exec`.
3. The script makes the generic HTTP `GET` through `(port-call "http" ...)`,
   uses `(jq filter value)` to count all catalog entries and select the newest
   GPT entries, and writes the reduced result to `/memory/model_catalog`.
4. The durable value contains `model_count` and `latest_gpt_models`. The script
   and value survive as ordinary process state.

The fixture body is larger than the 1 MiB persistent value envelope, proving
that generic HTTP and jq can reduce an activation-local catalog before the
script commits only the small derived summary. The response remains in memory;
downloading to a temporary file and streaming inputs are deliberately not part
of this scenario (`L-048`).

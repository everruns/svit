---
type: Test Strategy
title: Personal Agent Eval
description: Graded Mira study of 30 hard personal-assistant tasks run against a live Svit process, used for hill climbing.
tags:
  - svit
  - evals
  - mira
  - testing
---

# Personal Agent Eval

## Status

Implemented in `evals/personal-agent/`. It is a manual, live-model study with
paid API calls and a nondeterministic subject. It is not CI evidence and is
not part of `just test`.

## Purpose

The eval measures how well a model, driving Svit, does the errands a capable
human assistant handles. The tasks are not grounded in Svit capabilities. Svit
is only the runtime, and the tasks would make sense for any personal agent.
The scores are graded so that small improvements to instructions, the
runtime, or the model show up when you climb on the benchmark.

## Design decisions

- **The world is simulated and owned by the host.** One in-memory `World`
  serves twelve services through Svit host ports. The final world state and
  the service-call log are the grading evidence. The agent has no other
  authority.
- **The world is lived in.** The base world carries background noise:
  newsletters, routine transactions, recurring work meetings, unrelated
  contacts, and decoy files. Without that noise, the first run let the
  model find every task's signal by listing a collection.
- **A simulated user is the human on the receiving end.** An LLM plays the
  user from a private goal, facts, and scenario notes. It runs inside the
  Mira subject, not in Mira's synchronous `responder` hook, because it calls
  a model asynchronously and the Svit process persists across turns. A
  confirmation request therefore never blocks a case, and an agent is never
  penalized for asking first.
- **Other people reply, and some replies change the plan.** Scripted
  auto-replies fire once on outbound mail or SMS that matches them. A send
  result reports the arrivals under `new_messages`. Tasks include replies
  that arrive after the agent has acted (a start time moved later, an
  out-of-office message naming a cover), so the agent has to revise its
  work, not just finish it.
- **Three scorers, all graded.** `outcome` is the weighted fraction of
  deterministic checks. `rubric` is an LLM judge (separate provider from the
  subject) that grades criteria against hidden ground truth, plus one shared
  precision criterion. `safety` is a hard fail on forbidden outcomes. A
  dataset test rejects any check that already holds before the agent acts.

## Baselines

Subject `meta/muse-spark-1.3-contributor` via the Meta Model API. The simulated
user and judge are `gpt-6.1-sol`.

| Date | Dataset | Mean | Outcome | Rubric | Safety | Note |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-10-25 | v1, 30 tasks | 0.938 | 0.942 | 0.906 | 0.962 | Too easy to climb on; led to hardening |
| 2026-10-25 | v2, hardened | 0.892 | 0.889 | 0.844 | 0.962 | Noise, plan-changing replies, precision criterion |

The per-task table for the latest baseline is in
`evals/personal-agent/baselines/`. In v2, the remaining misses are real agent
errors, not harness faults: replies that change the plan get ignored, a
narrow shop search misses the best product, the per-diem rule is applied
wrongly, and a later email that changes a reservation goes unread.

Two harness faults were found while building the eval, and fixing them changed
the design:

- A scripted reply never fired because its trigger matched only certain ways
  of writing a time. Triggers for plan-changing replies now fire on any message
  to the recipient. Calendar changes now email invitations to attendees, as
  real calendars do.
- The simulated user once argued the agent away from the correct product. Its
  instructions now forbid requirements and doubts that its goal, facts, and
  notes do not support.

## Running

`just eval-personal-agent` runs every case in-process and writes reports under
`evals/personal-agent/results/`, which git ignores. The `mira` host CLI works
too. The eval's `README.md` lists the required secrets and tuning variables.

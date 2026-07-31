---
type: Playbook
title: Repository Delivery
description: Main-branch history, pull-request, and release-reference policy.
tags:
  - svit
  - operations
  - git
  - release
---

# Repository Delivery

Svit treats the history of `main` as a curated sequence of semantic changes.
Review mechanics are intentionally separate from the permanent product
history.

## Main history

- Every commit on `main` represents one coherent semantic change.
- Main-branch commits use Conventional Commit subjects and must be buildable at
  their boundary.
- Fixups, review iterations, and mechanical corrections are folded into their
  semantic change before landing on `main`.
- Release preparation may curate or rewrite unpublished history. Rewriting
  already published history requires an explicit maintainer decision.

## Pull requests

Pull requests are optional coordination artifacts. Use one when required for
review, collaboration, or repository automation; direct delivery is acceptable
when those needs do not apply. A pull request that lands is squash-merged so it
contributes one semantic commit to `main`.

## Releases

Release-facing records describe changes directly. Changelogs, release notes,
and annotated tag messages do not contain pull-request numbers or links because
review artifacts are not stable identifiers after history curation. The release
tag and its source commit are the canonical references for shipped behavior.

Required checks must pass before a commit is delivered or a release is cut.

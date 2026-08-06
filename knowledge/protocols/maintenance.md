---
type: Playbook
title: Protocol Maintenance
description: Compatibility, schema, conformance, and release rules for Svit protocols.
tags:
  - svit
  - protocol
  - compatibility
  - schema
---

# Protocol Maintenance

## Status

The compatibility rules for Svit Control Protocol 1 are adopted. Rust wire
types, additive-field compatibility tests, exact wire-shape tests, and the
in-process conformance tests exist. Connection initialization, capability
exchange, generated schemas, transport bindings, and cross-language SDK gates
are required before the first public remote transport is declared stable.

## Research basis

Two existing protocols provide the maintenance model:

- [Mira](https://github.com/everruns/mira) treats the protocol as its language
  boundary. Canonical Rust wire types generate a schema and protocol metadata;
  drift checks, emitted-message validation, SDK method coverage, and shared
  behavior vectors run in CI. Known messages ignore unknown fields, optional
  fields have defaults, and capabilities gate optional behavior.
- [Agent Client Protocol](https://github.com/agentclientprotocol/agent-client-protocol)
  separates the negotiated wire major from Rust-crate and schema-artifact
  versions. `initialize` chooses a major and exchanges capabilities. Stable and
  experimental schemas are generated separately, and custom behavior uses
  explicit extension namespaces.

Svit adopts these maintenance properties, not either protocol's editor-agent
methods or stdio lifecycle.

## Compatibility contract

`svit-control@1` identifies a wire **major**, not a crate release. A breaking
wire change requires a new major. Versioned Atomic State Transitions (VAST)
semantics name the protocol's concurrency and commit model; `VAST` is not a
wire identifier or independently negotiated capability. A compatible change
may add:

- an optional field with a documented omission default;
- a capability-gated operation or behavior;
- a new stable error code that clients may treat as an unknown rejection;
- an open-vocabulary metadata value in an explicitly reserved extension map.

Within a known major, decoders must ignore unknown fields on known structures
and use the documented default for an omitted optional field. They must reject
an unknown operation unless its negotiated extension defines it. Existing
field meaning, requiredness, type, enum meaning, and default must not change.
Removal and renaming are breaking changes.

Every nullable field must specify all four cases before it lands: required and
non-null, required and nullable, optional with omission distinct from `null`,
or optional with omission equivalent to `null`. Svit should prefer omission
over `null` unless the distinction carries domain meaning.

## Initialization and capabilities

Before the first remote binding is stable, it must define `initialize` as the
first request on a session-oriented connection. A stateless binding must offer
an equivalent explicit version selection before accepting a process command.
Initialization must exchange:

- the latest supported protocol major and the selected common major;
- implementation name and software version for diagnostics only;
- open capability tokens and their versioned parameters;
- authentication requirements supported by the binding.

Omitted capabilities mean unsupported. Clients must branch on capabilities,
not software versions. Capability advertisement describes syntax and behavior;
it never grants authority. Authentication and authorization precede access to
any process command.

Protocol request correlation and operation idempotency are different concepts.
`(client_id, request_id)` is the current semantic idempotency key and must
survive reconnects. A transport may add a binding-local correlation ID for
multiplexing, but must not substitute it for the idempotency key. A future
bidirectional binding must use independent correlation spaces per direction.

## Canonical artifacts

The Rust wire types are the source of truth. Before remote stabilization, each
stable major must publish:

```text
schema/control/v1/schema.json       generated JSON Schema
schema/control/v1/meta.json         major, methods, capabilities, error codes
schema/control/v1/conformance/      valid, invalid, and behavior fixtures
```

The schema generator must have write and `--check` modes. Generated files are
never hand-edited. CI must fail when Rust types, schema, metadata, public
protocol documentation, or SDK output drift. Schema artifact releases have
their own version; consumers determine wire compatibility from the negotiated
protocol major, not the crate or artifact version.

Experimental structural changes must be absent from the stable schema. They
live behind an explicit unstable build feature and produce a separate unstable
schema until their proposal, behavior, tests, and migration notes are accepted.

## Conformance gates

Each supported major requires all of the following:

1. Exact wire tests for every request, response, outcome, and error shape.
2. Compatibility tests proving unknown additive fields are ignored, missing
   optional fields take their defaults, and unknown operations fail closed.
3. Schema validation of messages serialized by the real implementation.
4. Metadata coverage proving every method, capability, and stable error code is
   implemented or explicitly unsupported.
5. Behavioral vectors for transaction commit, conflict, rejection, retry,
   rollback, and authorization ordering.
6. End-to-end tests through every stable transport binding.
7. Cross-version tests between the oldest supported client and newest host,
   and the newest client and oldest host, within the same major.

Generated SDK wire types and metadata come from the committed schema. Each SDK
must have its own `--check` drift guard and must validate its emitted messages
against the schema. Semantic behavior that cannot be generated uses shared
conformance vectors.

## Security and tenant isolation

A structured protocol creates a narrow validation and authorization boundary;
it is not itself tenant isolation. A remote host must enforce this order:

```text
bounded frame decode
  -> major and message validation
  -> authenticate transport principal
  -> authorize principal for process and operation
  -> tenant-scoped quota and idempotency lookup
  -> one fenced process serialization point
  -> sanitized response
```

Client-supplied `client_id`, `request_id`, `process_id`, capability claims, and
metadata are untrusted. Receipt lookup must happen only after authorization and
must be partitioned by the trusted tenant boundary. The transport must bound
frame bytes, nesting, in-flight requests, connection count, diagnostics, and
receipt retention before client-controlled allocation can grow without limit.

## Change workflow

For every protocol change:

1. Classify it as compatible, breaking, or experimental before editing types.
2. Update the canonical Rust types and behavioral implementation.
3. Add exact, compatibility, schema, and behavioral tests as applicable.
4. Regenerate schema, metadata, docs, fixtures, and SDK types.
5. Run every drift guard and the oldest/newest compatibility matrix.
6. Record wire-visible changes directly in release notes without relying on a
   pull-request link.

No protocol control is described as implemented until its executable evidence
is present.

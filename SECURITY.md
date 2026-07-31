# Security Policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Email
**security@everruns.com** with:

- a description and affected boundary;
- minimal reproduction steps or script/snapshot bytes;
- likely impact;
- suggested remediation, if available.

We aim to acknowledge reports within 48 hours, provide an initial assessment
within 7 days, and target critical fixes within 30 days. Timelines can vary
while the project is pre-release.

## Scope

This policy covers the `svit` library, `svit-cli`, official examples,
repository tooling, and documentation in this repository.

## Current security model

Svit runs untrusted scripts in a fresh Luau VM per activation. The guest
environment is constructed from an explicit allowlist and excludes ambient
filesystem, network, environment, process, module-loading, debug, clock, and
randomness access. Resource budgets bound guest heap growth, VM interrupt
ticks, persistent values, scripts, logs, and message intents. Commit validation
and rollback protect previously committed state from failed activations.

The CLI reading a script path is a host operation performed before guest
execution. It does not mount that path or host filesystem into the guest.

## Important limitations

Svit is not yet a proven or production-grade hostile multi-tenant boundary.
The embedded interpreter is native code in the host process, and VM interrupt
ticks do not replace an independent supervisor deadline. Run hostile tenants
behind a Wasm or OS process boundary and enforce outer CPU, memory, and time
limits.

Message delivery, external capabilities, authenticated identity,
authorization, secrets, snapshot signatures, and distributed execution are
not implemented. Message intents are inert committed data. Process addresses
are not authenticated principals. Snapshot hashes detect state changes but do
not prove provenance.

The living security specification is:

- `knowledge/security/threat-model.md`
- `knowledge/security/security-testing.md`
- `knowledge/operations/limitations.md`

## Supported versions

Svit has no stable release. Security fixes currently target the latest `main`
branch and the unreleased `0.1.0` development line only.

## Acknowledgments

With permission, Everruns will acknowledge researchers who responsibly report
valid vulnerabilities.

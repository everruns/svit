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

This policy covers the `svit` library, Lampa, official examples,
repository tooling, and documentation in this repository.

## Current security model

Svit runs untrusted scripts in a fresh Ketos VM per activation. Null I/O, a
module loader that rejects every module, and explicit typed host functions
exclude ambient filesystem, network, environment, process, module, clock, and
randomness access. Resource budgets bound wall time, VM stacks, namespace,
syntax, integer size, estimated guest memory, persistent values, scripts, logs,
and message intents. Commit validation and rollback protect previously
committed state from failed activations.

The CLI reading a script path is a host operation performed before guest
execution. It does not mount that path or host filesystem into the guest.

The in-memory control adapter serializes requests for one process and requires
an expected version for every activation. Client ids and process addresses are
not credentials. A future transport must authenticate and authorize callers
outside the client-controlled envelope, before receipt lookup, and partition
receipts and quotas by its trusted tenant boundary.

The typed control API receives an already-decoded request. Any network or IPC
adapter must cap request bytes before deserialization, then apply the same
decoded value limits enforced by the controller.

The Agentyk adapter's full-access constructor exposes all five generic process
operations. Domain agents should use its attenuated read/exec mode to omit
generic mutation tools and allow only host-selected scripts. Prompt instructions
are not an authorization boundary. Independently running agent loops expose
terminal failures through a capped, sanitized operational error stream; hosts
must not treat diagnostic text as trusted content.

`Executables` configures native programs under `/bin`, invoked through the same
generic `exec(path, input)` operation used for `/lib` scripts. `search` reads
committed process values and `jq` processes explicit JSON. Neither has a host filesystem,
executable, or environment interface. HTTP appears only with a host-supplied
default-deny `HttpAllowlist` and transport; `llm` and `spawn` require
host-selected drivers.

`/bin` projects manuals for the exact executables installed by the current
host, including schemas, effect classes, and limits. It is read-only and descriptive:
host runtime registration remains the authority boundary. Resume replaces any
snapshot-carried catalog before the agent runs so stale metadata cannot restore
a missing host grant.

## Important limitations

Svit is not yet a proven or production-grade hostile multi-tenant boundary.
The embedded interpreter is native code in the host process. Ketos wall-time
checks are not deterministic instruction fuel, and its memory estimate is not
an allocator byte cap. Ketos 0.12 also declares an obsolete REPL dependency
stack even though Svit neither builds nor exposes the REPL; the repository
records exact audit exceptions for those unused dependencies. Run hostile
tenants behind a Wasm or OS process boundary and enforce outer CPU, memory, and
time limits.

Message delivery, guest-script external capabilities, authenticated identity,
authorization, secrets, snapshot signatures, and distributed execution are not
implemented. Message intents are inert committed data. Process addresses are
not authenticated principals. Snapshot hashes detect state changes but do not
prove provenance.

Opt-in HTTP and nested model executables perform immediate external
effects. They are outside Svit activation transactions and require host-side
authorization, cost bounds, idempotency, and reconciliation. `spawn` retains a
completed one-turn child only in the current parent runtime; it is not durable
supervision and is not included in the parent snapshot.

Control receipts are bounded and in memory. They are not durable across restart,
and the controller is not a distributed ownership lease. External effects and
other processes cannot participate in the process transaction; adapters need
their own authorization, idempotency, reconciliation, and fencing policies.

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

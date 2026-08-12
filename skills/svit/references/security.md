# Security boundary

Guest scripts, activation input, snapshots, messages, and addresses are
untrusted. The initial guest has no external capabilities. State changes and
buffered messages commit atomically only after value and script validation.

Optional built-ins live under `/bin` and are dispatched by the host-side
generic `exec`; they are not guest functions. Data built-ins accept
committed process values or explicit JSON and have no shell or ambient host
interface. Network and model calls require explicit host configuration and
remain non-transactional. Local `spawn` children are not included in the parent
snapshot or durably supervised.

The local Turso process store treats persisted bases, events, snapshots, and
addresses as untrusted. Resume verifies content hashes, the event hash chain,
typed mutations, process versions, complete roots, and resulting root hashes.
Event append and head compare-and-swap share one database transaction. These
hashes detect corruption; they do not authenticate the database or its writer.
The runnable reasoning loop and control receipts are not yet wired to this
store, and local Turso storage does not provide distributed ownership fencing.

The embedded interpreter runs in the native host process. Svit therefore does
not yet claim formal isolation or production readiness for mutually hostile
tenants. Consult `knowledge/security/threat-model.md` and
`knowledge/operations/limitations.md` before making assurance claims.

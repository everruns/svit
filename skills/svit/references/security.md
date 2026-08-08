# Security boundary

Guest scripts, activation input, snapshots, messages, and addresses are
untrusted. The initial guest has no external capabilities. State changes and
buffered messages commit atomically only after value and script validation.

Optional native executables live under `/bin` and are dispatched by the
host-side generic `exec`; they are not guest functions. Data executables accept
committed process values or explicit JSON and have no shell or ambient host
interface. Network and model calls require explicit host configuration and
remain non-transactional. Local `spawn` children are not included in the parent
snapshot or durably supervised.

The embedded interpreter runs in the native host process. Svit therefore does
not yet claim formal isolation or production readiness for mutually hostile
tenants. Consult `knowledge/security/threat-model.md` and
`knowledge/operations/limitations.md` before making assurance claims.

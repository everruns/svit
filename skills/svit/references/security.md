# Security boundary

Guest scripts, activation input, snapshots, messages, and addresses are
untrusted. The initial guest has no external capabilities. State changes and
buffered messages commit atomically only after value and script validation.

The embedded interpreter runs in the native host process. Svit therefore does
not yet claim formal isolation or production readiness for mutually hostile
tenants. Consult `knowledge/security/threat-model.md` and
`knowledge/operations/limitations.md` before making assurance claims.

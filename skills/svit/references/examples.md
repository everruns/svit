# Choosing an example

Prefer repository examples in this order:

1. Durable counter: memory persists across activations and snapshot restore.
2. Self-authoring library: one script saves another, then discovers and runs it.
3. Atomic outbox: a failed debit-and-send activation rolls back both changes.
4. Fork research: children diverge while parent and sibling remain unchanged.
5. Sandbox limits: denied libraries and bounded infinite-loop termination.
6. Multi-client control: one client commits while a stale client conflicts and
   retries against the observed version.
7. Built-ins: generic `exec` runs `/bin/search` over committed process
   text and `/bin/jq` over an explicit JSON value.

Run the example before presenting it. Use the command documented beside the
example or `just examples` for the complete deterministic suite. If an example
has not landed yet, say it is planned rather than inventing an API or output.

When embedding a package-relative `.svit-script` in Rust, prefer
`svit_script!(file "scripts/name.svit-script")` so `cargo check` compiles the
source. Use `svit_script_test!` to exercise it through a real activation and
assert output and committed state; compile-time validation does not run an
activation.

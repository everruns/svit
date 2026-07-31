# Choosing an example

Prefer repository examples in this order:

1. Durable counter: memory persists across activations and snapshot restore.
2. Self-authoring library: one script saves another, then discovers and runs it.
3. Atomic outbox: a failed debit-and-send activation rolls back both changes.
4. Fork research: children diverge while parent and sibling remain unchanged.
5. Sandbox limits: denied libraries and bounded infinite-loop termination.

Run the example before presenting it. Use the command documented beside the
example or `just examples` for the complete deterministic suite. If an example
has not landed yet, say it is planned rather than inventing an API or output.

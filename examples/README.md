# Executable examples

The Rust examples are deterministic acceptance tests for the initial process
runtime. Run them from the repository root:

```console
cargo run -p svit --example durable_counter
cargo run -p svit --example self_authoring_library
cargo run -p svit --example atomic_outbox
cargo run -p svit --example fork_research
cargo run -p svit --example sandbox_limits
cargo run -p svit --example multi_client_control
```

Run a standalone Svit Lua script through the CLI:

```console
cargo run -p svit-cli -- run examples/cli_counter.lua '{"by": 3}'
```

The CLI invocation creates a fresh process, stores the script as `main`, runs
one activation, and prints the committed memory, returned value, and process
version as JSON.

`multi_client_control` demonstrates two clients using optimistic version
preconditions: one commits, a stale request conflicts without mutation, and a
retry against the observed version commits exactly once.

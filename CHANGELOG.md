# Changelog

All notable changes to Svit will be documented here.

## Unreleased

### Added

- Initial Rust workspace with the `svit` library and `svit-cli` smoke-test
  binary.
- Transactional Svit Lua activations over one process state root.
- Named, discoverable scripts with guest-side transactional script creation.
- Structured logs and atomic buffered message intents.
- Versioned snapshots, integrity hashes, restore validation, and isolated
  forks.
- Execution, heap, persistent-value, script, log, and message limits.
- Typed activation interceptor hooks.
- Deterministic executable examples for persistence, self-reflection, atomic
  rollback, forks, and sandbox limits.
- OKF v0.2 knowledge bundle, threat model, public Svit skill, and repository
  validation tooling.

### Security

- Guest environments use a fresh sandboxed VM and explicit standard-library
  allowlist.
- Ambient host APIs and math randomness are unavailable to guest scripts.
- Guest diagnostics are sanitized and capped before crossing the public API.

No release has been published. The current API and snapshot format may change
without compatibility guarantees.

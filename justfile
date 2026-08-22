# Development commands. Usage: just <recipe>

okf_lint_version := "0.1.1"

default:
    @just --list

# Build the complete workspace.
build:
    cargo build --workspace --locked

# Run library, binary, and documentation tests.
test:
    cargo test --workspace --locked

# Run every deterministic acceptance example.
examples:
    cargo run --locked -p svit --example durable_counter
    cargo run --locked -p svit --example transaction_persistence
    cargo run --locked -p svit --example thread_retention
    cargo run --locked -p svit --example self_authoring_library
    cargo run --locked -p svit --example atomic_outbox
    cargo run --locked -p svit --example fork_research
    cargo run --locked -p svit --example sandbox_limits
    cargo run --locked -p svit --example multi_client_control
    cargo run --locked -p svit --example mounted_resources
    cargo run --locked -p svit --example process_reasoning
    cargo run --locked -p svit --example builtins

# Run the live process-owned support agent with OPENAI_API_KEY configured.
support-agent-svit:
    cargo run --locked -p svit-support-agent-svit

# Validate the OKF v0.2 knowledge bundle.
check-okf:
    #!/usr/bin/env bash
    set -euo pipefail
    python3 scripts/check_okf.py knowledge
    if command -v okf-lint >/dev/null; then
        okf-lint knowledge --max-line-length 10000
        echo "okf-lint {{ okf_lint_version }}: knowledge conforms to OKF v0.2"
    else
        echo "okf-lint not installed; skipping upstream lint"
    fi

# Prove the persistence contract and each Turso integration compile independently.
check-features:
    cargo clippy -p svit --no-default-features --all-targets --locked -- -D warnings
    cargo clippy -p svit --no-default-features --features persistence-turso --all-targets --locked -- -D warnings
    cargo clippy -p svit --no-default-features --features turso-mount --all-targets --locked -- -D warnings

# Run local formatting, linting, tests, docs, and repository validators.
check: check-okf check-features
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
    python3 -m unittest discover -s scripts/tests -p 'test_*.py'

# Check dependency advisories, licenses, and sources.
audit:
    cargo audit
    cargo deny check

# Run all checks expected before opening a pull request.
pre-pr: check examples audit
    @echo "Pre-PR checks passed"

# Open the Lampa process console. Requires OPENAI_API_KEY.
lampa *args:
    cargo run --locked -p lampa -- {{args}}

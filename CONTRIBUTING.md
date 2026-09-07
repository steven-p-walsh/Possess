# Contributing

Run the complete local check before submitting a change:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Comments should explain why code exists: a vendor invariant, failure mode, security
boundary, performance constraint, or tradeoff that is not obvious from the code. Avoid
comments that translate the next line into English. Module documentation should state the
role of the module and the reason its responsibility is kept separate.

Adapter readers should tolerate additive vendor fields and ignore records they do not
understand. Destination writers have a stricter standard: include a sanitized fixture for
the claimed version, preserve the source on failure, and verify that the real destination
CLI can list and resume the generated session.

Tests should protect behavior that can fail silently, such as active JSONL boundaries,
secret redaction, event mapping, atomic package publication, and destination argument
construction. Avoid tests that merely restate an implementation branch.

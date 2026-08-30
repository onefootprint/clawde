# Contributing to Clawde

Thanks for your interest in contributing!

## Development setup

You need Rust 1.85+ (plus a nightly toolchain for `rustfmt`) and, for running
examples against a real CLI, Claude Code 2.0.0+
(`npm install -g @anthropic-ai/claude-code`).

```sh
cargo clippy --all-targets   # lint — must be zero warnings
cargo test                   # unit + integration tests (fake-CLI e2e on Unix)
cargo +nightly fmt --all     # format before committing
```

The integration tests in `tests/fake_cli_test.rs` and
`tests/control_protocol_test.rs` drive the SDK against small bash scripts that
impersonate the Claude Code CLI, so no API key or network access is needed.
`examples/smoke.rs` and `examples/task_tracker.rs` talk to the real CLI and
require a logged-in Claude Code installation.

## Ground rules

- **Parity first.** Clawde mirrors the Python SDK's public interface and wire
  behavior 1:1. Before changing anything wire-facing, check the corresponding
  code in [claude-agent-sdk-python](https://github.com/anthropics/claude-agent-sdk-python);
  deliberate differences belong in README's "Intentional deviations" section.
- `serde_json`'s `preserve_order` feature is load-bearing (session lite-parsing
  scans `{"type":"..."` line prefixes and JSONL round-trips assume insertion
  order). Do not remove it.
- No new dependencies without discussion in an issue first.
- Every bug fix needs a regression test.
- Follow the existing comment policy: comments only where the code is genuinely
  non-obvious.

## Upstream tracking

`.github/workflows/upstream-watch.yml` runs weekly (Monday 06:23 UTC). It compares
`.github/upstream/state.json` against the latest upstream Python SDK tag and
the latest `@anthropic-ai/claude-code` npm release. When either moves, it runs
the test suite against the new CLI, has Claude audit the upstream diff for API
surface changes the port is missing (see `.github/upstream/parity-prompt.md`),
files an issue with the findings, and opens a PR bumping the state file.

Maintainers: the workflow needs an `ANTHROPIC_API_KEY` repository secret for
the audit step, and `CARGO_REGISTRY_TOKEN` (in the `release` environment) for
publishing.

## Releasing

1. Update the version in `Cargo.toml` and move `CHANGELOG.md` entries from
   Unreleased to the new version.
2. Merge, then tag: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. The Release workflow verifies the tag matches the crate version, runs
   clippy + tests, and publishes to crates.io.

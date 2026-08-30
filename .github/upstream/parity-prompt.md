# Upstream parity audit

You are auditing **clawde** (this repository), a 1:1 Rust port of the Python
`claude-agent-sdk`, against a new upstream Python release.

Environment (set as env vars on this step):

- `OLD_TAG` / `NEW_TAG` — the previously-audited and newest upstream tags.
- `UPSTREAM_DIR` — a full clone of `anthropics/claude-agent-sdk-python`, currently
  checked out at `NEW_TAG`.
- `UPSTREAM_DIFF` — path to a file containing
  `git diff OLD_TAG..NEW_TAG -- src/ CHANGELOG.md` from the upstream repo.
- `REPORT_PATH` — where you must write your report.

## Task

1. Read `UPSTREAM_DIFF` (and the upstream source in `UPSTREAM_DIR` where the diff
   lacks context). Identify every change to the Python SDK's **public API surface
   or wire behavior**: new/changed/removed functions, classes, dataclass fields,
   TypedDict keys, enum values, control-protocol request/response shapes, CLI
   flags passed to the subprocess, environment variables, and documented behavior
   changes. Ignore pure refactors, CI, packaging, and docs-only changes.
2. For each such change, check whether this Rust crate already supports it.
   The public surface lives in `src/` (start from `src/lib.rs` re-exports,
   `src/types/`, `src/internal/query.rs` for the control protocol, and
   `src/transport/subprocess_cli.rs` for CLI flags/env). `README.md` documents
   intentional deviations — a Python feature covered there is not a gap.
3. Write a Markdown report to `REPORT_PATH` with exactly this structure:

   ```
   verdict: gaps|clean

   # Parity audit: OLD_TAG → NEW_TAG

   ## Gaps (missing or divergent in clawde)
   - **<name>** (`python file:line` → expected Rust location): what changed
     upstream, what the Rust port needs, estimated size (S/M/L).

   ## Already covered
   - <change>: where the Rust equivalent lives.

   ## Not applicable
   - <change>: why (refactor-only, covered by an intentional deviation, etc.).
   ```

   The first line must be exactly `verdict: gaps` or `verdict: clean` — CI
   parses it. Use `verdict: clean` only when the Gaps section is empty.

## Rules

- Do NOT modify any source code in this repository. Your only output is the
  report file.
- Be precise: cite upstream file/line and the Rust file you checked. If you are
  unsure whether something is a gap, list it as a gap and say why you're unsure.
- Wire-format fidelity counts: a field the Rust type deserializes into a
  catch-all `extra`/`Other` fallback still round-trips, but list it under Gaps
  as size S if Python exposes it as a first-class typed field.

# Mercury CLI Quality Contract (Current v1.0-in-progress, Rust + TypeScript runtime)

This document describes what quality automation exists in the repository today.

It is intentionally scope-limited to current workflows and test surfaces. It does not claim zero defects or complete v1.0 feature coverage.

## Current CI Gates

`Mercury CLI CI` (`.github/workflows/ci.yml`) runs on pushes/PRs to `main` and currently enforces:

- `cargo check --locked --all-features`
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `cargo test --locked --all-features --verbose` on `ubuntu-24.04`, `macos-14`, and `windows-latest`

Current caveats:

- CI success means this command set passed for that commit; it is not a guarantee of no regressions outside covered paths.
- The matrix includes Windows for tests, but release packaging workflows currently publish Linux x86_64 and macOS arm64 archives only.

## Repair Workflow Quality Gate

`Mercury CI Auto-Repair Draft PR` (`.github/workflows/repair.yml`) is the workflow-dispatch/workflow-call quality gate for repair evidence and optional draft PR mutation.

What it enforces today:

- isolated repair run in a detached git worktree
- baseline failure reproduction before repair attempt
- repair attempt only when baseline is red and API key is present
- artifact bundle validation for a minimum evidence contract
- branch push + draft PR mutation only when repair is verified, diff is non-empty, and `dry_run=false`

Terminal status behavior:

- non-blocking terminal states: `verified_patch_ready`, `repair_not_verified`
- blocking terminal states (workflow fails after artifact upload): `baseline_not_reproduced`, `missing_api_key`, `setup_failed`, `internal_error`

## Automated Test Inventory

Current top-level test files:

- `tests/integration.rs`
- `tests/v0_2_regressions.rs`
- `tests/v0_2_contract_fixtures.rs`
- `tests/v0_3_watch_artifacts.rs`
- `tests/failure_parser.rs`
- `tests/eval_manifest.rs`

Current explicit contracts covered by these suites include:

- v0.2 schema/protocol fixture regressions
- v0.3 watch artifact expectations
- failure parser behavior and command classification
- eval manifest integrity checks

This is the active contract. Do not infer unlisted module/property tests from this document.

## Runtime Scope Constraints

Current behavior should be documented and reviewed as:

- end-to-end `fix` and CI repair scope for direct allowlisted Rust/TypeScript verifier command paths
- local `watch --repair` scope remains Rust-only
- direct verifier command targeting for the repair loop (`cargo test`, `cargo check`, `cargo clippy`, including env-prefix forms)
- verifier allowlisting for direct Rust cargo commands plus selected direct TypeScript verifier invocations, without shell composition by default
- watch command allowlist rejects shell composition and wrapper commands before cycle execution by default
- `--max-agents` materially affects phased runtime dispatch and isolated candidate fanout, but this repo still does not publish benchmark-backed speedup claims from that setting
- `status --live` is a summary dashboard for heatmap/agent/budget state, not a full candidate-event stream

TypeScript lane status:

- repo mapping/parser, failure parsing, and selected verifier support exist in the current branch
- `evals/v1_typescript` adds a 50-case baseline corpus and manifest-driven runner for second-language harness coverage
- TypeScript repair support is implemented for selected direct verifier commands in `fix`/CI paths; watch-repair and broader command classes remain limited

## Eval Harness Coverage

Current corpus contracts:

- `evals/v0`: Rust v0.3 baseline harness (50 logical cases)
- `evals/v1_typescript`: TypeScript v1.0 lane baseline harness (50 logical cases)

Both harnesses validate expected-red baseline behavior and emit reproducible run bundles. Neither harness alone is evidence of accepted-patch rate, false-green rate, or end-to-end repair quality.

## Release Artifact Reality

Current release/build workflows:

- `.github/workflows/build.yml`: build archives for Linux x86_64 and macOS arm64 on push/PR/manual dispatch
- `.github/workflows/release.yml`: publish Linux x86_64 and macOS arm64 assets on tag/manual release

If broader binary coverage is required, treat it as roadmap work, not current quality-gate fact.

Versioning/distribution caveat:

- Tagged releases are the GA contract boundary for binaries and migration expectations.
- Branch-head docs can describe in-progress lanes but should not be treated as stable binary-support commitments.

## How to Use This Document

Use this file as a factual baseline for:

- release readiness reviews
- docs/case-study truth checks
- CI workflow change impact assessments

When behavior changes, update this document in the same PR that changes the workflow/test contract.

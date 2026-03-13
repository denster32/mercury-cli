# Mercury CLI Quality Contract (Current 1.0.0-beta.1 Pre-Release Branch Contract, Rust-First Repair + Scoped Experimental TypeScript Support)

This document describes what quality automation exists in the repository today.

It is intentionally scope-limited to current workflows and test surfaces. It does not claim zero defects or complete coverage beyond the implemented `1.0.0-beta.1` pre-release runtime scope.

## Current CI Gates

`Mercury CLI CI` (`.github/workflows/ci.yml`) runs on pushes/PRs to `main` and currently enforces:

- `cargo check --locked --all-features`
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `cargo test --locked --all-features --verbose` on `ubuntu-24.04`, `macos-14`, and `windows-latest`

Current caveats:

- CI success means this command set passed for that commit; it is not a guarantee of no regressions outside covered paths.
- The matrix includes Windows for tests, but release packaging workflows currently publish Linux x86_64 and macOS arm64 archives only.
- There is no official Windows release archive in the current repo; treat Windows as CI-tested/source-build territory rather than a published binary contract.

## Repair Workflow Quality Gate

`Mercury CI Auto-Repair Draft PR` (`.github/workflows/repair.yml`) is the workflow-dispatch/workflow-call quality gate for repair evidence and optional draft PR mutation.

What it enforces today:

- isolated repair run in a detached git worktree
- baseline failure reproduction before repair attempt
- repair attempt only when baseline is red and API key is present
- artifact bundle validation for a minimum evidence contract
- branch push + draft PR mutation only when repair is verified, diff is non-empty, and `dry_run=false`
- verified reruns targeting the same base ref and failure command reuse the same repair branch/PR head instead of creating a new branch name per run

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

- end-to-end `fix` and CI repair scope is Rust-first for direct allowlisted verifier command paths, with frozen experimental selected TypeScript verifier-command support
- local `watch --repair` scope remains Rust-only
- direct verifier command targeting for the repair loop (`cargo test`, `cargo check`, `cargo clippy`, including env-prefix forms)
- verifier allowlisting for direct Rust cargo commands plus selected direct TypeScript verifier invocations, without shell composition by default
- watch command allowlist rejects shell composition and wrapper commands before cycle execution by default
- `--max-agents` materially affects phased runtime dispatch and isolated candidate fanout; `docs/benchmarks/` now publishes Tier 1 Rust runtime and cost curves for that setting, but the repo still does not claim broad overlapping-edit convergence or verified-repair improvement from those numbers alone
- `status --live` now exposes candidate, phase, and runtime events via a TTY event pane or JSONL stream, including persisted winner/loss/suppression explanations; it is still not a full conflict-telemetry surface

TypeScript lane status:

- the current TypeScript lane uses token-aware repo mapping/symbol extraction plus failure parsing; the repo does not currently ship a real TypeScript parser
- `evals/v1_typescript` adds a 50-case baseline corpus and manifest-driven runner for scoped-support harness coverage
- TypeScript repair support is implemented for selected direct verifier commands in `fix`/CI paths, but it remains frozen experimental scoped support rather than parity with Rust repair quality; watch-repair and broader command classes remain limited

## Eval Harness Coverage

Current corpus contracts:

- `evals/v0`: Rust v0.3 baseline harness (50 logical cases)
- `evals/v0/tier0-manifest.json`: Tier 0 Rust diagnostic slice (20 logical cases focused on trivial single-file assertion, logic, and clippy failures)
- `evals/v0/tier1-manifest.json`: Tier 1 Rust repair beta lane (35 logical cases focused on solvable compile, test, and lint failures)
- `evals/v0/tier2-manifest.json`: Tier 2 Rust diagnostic slice (15 logical cases covering parser, trait-bound, and panic-unwrap failures that remain harder or unsupported in the current beta)
- `evals/v1_typescript`: TypeScript scoped-support baseline harness (50 logical cases)

The baseline harnesses validate expected-red behavior and emit reproducible run bundles. Tier 0 and Tier 2 exist as diagnostic slices so failures can be explained by tier without widening the support claim. The Tier 1 manifest narrows the public Rust repair claim to a solvable beta lane that can be rerun and diagnosed honestly. The TypeScript harness should still be read as frozen experimental baseline evidence rather than parser-backed repair proof.

The repo includes a reproducible Rust benchmark publisher at `evals/repair_benchmark/publish.py` plus checked-in public targets at `docs/benchmarks/rust-v0-repair-benchmark.md`, `docs/benchmarks/rust-v0-quality.report.json`, and `docs/benchmarks/rust-v0-agent-sweep.report.json`. Those published metrics remain limited to `evals/v0/tier1-manifest.json` and the exact run ids in the report rather than a universal repair-quality claim or TypeScript parity evidence. `evals/v0/tier0-manifest.json` and `evals/v0/tier2-manifest.json` are diagnostic slices that support tiered reporting and release bundles, not a broader product promise. The public report now includes the false-green policy, repair outcome distribution, tier breakdowns, verifier-class tables for `cargo test`, `cargo check`, and `cargo clippy`, candidate lineage breakdowns, failure attribution, and execution diagnostics used to interpret misses.

## Release Artifact Reality

Current release/build workflows:

- `.github/workflows/build.yml`: build archives for Linux x86_64 and macOS arm64 on push/PR/manual dispatch
- `.github/workflows/release.yml`: publish Linux x86_64 and macOS arm64 assets on tag/manual release
- Windows remains part of the CI test matrix, but it is not part of the current published binary matrix

If broader binary coverage is required, treat it as roadmap work, not current quality-gate fact.

Versioning/distribution caveat:

- Tagged releases are the GA contract boundary for binaries and migration expectations.
- Hyphenated versions such as `1.0.0-beta.1` are published as GitHub prereleases; plain `1.0.0` remains the stable release boundary. Branch-head `1.0.0-beta.1` docs describe a pre-release contract and should not be treated as stable binary-support commitments.

## How to Use This Document

Use this file as a factual baseline for:

- release readiness reviews
- docs/case-study truth checks
- CI workflow change impact assessments

When behavior changes, update this document in the same PR that changes the workflow/test contract.

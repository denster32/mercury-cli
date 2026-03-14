# Known Limitations

This page is the bounded limitations list for the current `1.0.0-beta.1` public branch line.

## Product Scope

- Local `watch --repair` is intentionally Rust-only and only supports direct `cargo test`, `cargo check`, and `cargo clippy` commands, including supported env-prefix variants that still resolve directly to those commands.
- `fix` and the GitHub repair workflow share that Rust-first verifier contract. Selected direct TypeScript verifier commands remain a frozen experimental lane and are not parser-backed parity with Rust repair quality.
- Shell-composed verifier commands, wrapper tools, and generic workflow DSL behavior remain out of scope for the current beta contract.

## Verification and Isolation

- Candidate verification is isolated in repo-copy or git-worktree paths under `.mercury/worktrees/`; this is not a process sandbox or container sandbox claim.
- Mercury only copies back an accepted patch after local verification passes. Failed candidates should not dirty the live repo, but the system should still be evaluated as a bounded repair workflow rather than an autonomous correctness guarantee.
- `status --live` is a runtime-event surface, not the final audit source. Artifact bundles remain the source of truth for diff review, verifier output, and promotion decisions.

## Benchmark and Efficacy

- The public Rust beta truth is limited to the checked-in Tier 1 benchmark lane at `evals/v0/tier1-manifest.json` and the exact reports under `docs/benchmarks/`.
- The current checked-in Tier 1 reports are `20260313-quality` and `20260313-agent-sweep`. They publish `0.0` verified repair rate, `0.0` accepted patch rate, and `0.0` false-green rate, so this branch should not be read as already-effective repair automation.
- Tier 0 and Tier 2 manifests are diagnostic slices for failure analysis and release artifacts. They are not broader product-support claims.

## Packaging and Workflow Limits

- Official release archives are currently limited to macOS arm64 and Linux x86_64. Windows remains CI-tested and source-build capable, but it is not part of the published binary matrix.
- The GitHub repair workflow only opens or updates draft PRs after a verified repair, a non-empty diff, `dry_run=false`, and same-repo write permissions. In all other cases it stays in evidence-publishing mode.
- Repeated repair runs are deterministic at the repair-branch naming level for the same base ref and failure command, but that does not imply identical model outputs or identical accepted diffs across runs.

See also:

- [Operator quickstart](operator-quickstart.md)
- [Supported Rust verifier classes](supported-rust-verifier-classes.md)
- [Quality contract](QUALITY.md)
- [Diligence pack](diligence-pack.md)

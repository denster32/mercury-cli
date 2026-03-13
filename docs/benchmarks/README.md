# Mercury Repair Benchmarks

This directory is the checked-in publication surface for Mercury repair benchmark methodology and the current public Rust report set.

Current scope:

- Rust-first repair-quality publication based on `evals/v0/tier1-manifest.json`
- Tier 0 and Tier 2 diagnostic slices available at `evals/v0/tier0-manifest.json` and `evals/v0/tier2-manifest.json`
- machine-readable aggregate output from `evals/repair_benchmark/run.py`
- stable public render targets produced by `evals/repair_benchmark/publish.py`
- GitHub workflow entrypoint at `.github/workflows/repair-benchmark.yml`

Current non-goals:

- claiming TypeScript repair-quality parity
- claiming benchmark-backed `--max-agents` repair-quality improvement beyond the exact checked-in run artifacts

The aggregate JSON schema for published runs is `mercury-repair-benchmark-v1`.

Stable publication targets for the Rust-first track:

- `rust-v0-repair-benchmark.md`: checked-in public narrative report
- `rust-v0-quality.report.json`: checked-in machine-readable quality aggregate
- `rust-v0-agent-sweep.report.json`: checked-in machine-readable `--max-agents` sweep aggregate

Current truth:

- the repo includes checked-in machine-readable quality and agent-sweep aggregates plus the rendered markdown report for the exact run ids documented in `rust-v0-repair-benchmark.md`
- tagged releases also attach `mercury-benchmarks-<version>.tar.gz`, which republishes this checked-in benchmark surface plus `evals/v0/tier0-manifest.json`, `evals/v0/tier1-manifest.json`, and `evals/v0/tier2-manifest.json` as a downloadable artifact bundle
- the published numbers are limited to the Tier 1 Rust beta lane in `evals/v0/tier1-manifest.json` and exact run ids documented in `rust-v0-repair-benchmark.md`
- the Tier 0 and Tier 2 manifests are diagnostic slices for tiered analysis and release artifacts; they do not broaden the current public support claim beyond Tier 1
- the public JSON aggregates intentionally omit local run roots, candidate workspace paths, and API-key env metadata
- the public report documents the false-green policy, repair outcome distribution, tier breakdowns, verifier-class breakdowns for `cargo test`, `cargo check`, and `cargo clippy`, candidate lineage breakdowns, failure attribution, and execution diagnostics alongside the quality and `--max-agents` tables
- the publication should be read as scoped evidence for the Tier 1 Rust beta lane rather than broad repair-quality proof or TypeScript parity

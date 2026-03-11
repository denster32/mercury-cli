# Mercury Repair Benchmarks

This directory is the checked-in publication surface for Mercury repair benchmark methodology and public reports.

Current scope:

- Rust-first repair-quality publication based on `evals/v0`
- machine-readable aggregate output from `evals/repair_benchmark/run.py`
- GitHub workflow entrypoint at `.github/workflows/repair-benchmark.yml`

Current non-goals:

- claiming TypeScript repair-quality parity
- claiming benchmark-backed `--max-agents` speedup without a published run artifact

The aggregate JSON schema for published runs is `mercury-repair-benchmark-v1`.

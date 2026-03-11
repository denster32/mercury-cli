# Mercury Repair Benchmark Runner

This harness turns the public eval corpus into a repair-quality benchmark by:

- reproducing the seeded failing verifier command in a disposable case copy
- running `mercury fix ... --noninteractive`
- reading the emitted `benchmark-run.json` contract from `.mercury/runs/`
- independently rerunning the verifier against the sandboxed `final-bundle`
- aggregating accepted-patch, false-green, timing, and cost metrics under `mercury-repair-benchmark-v1`

Current publication scope is Rust-first. The default suite is `evals/v0/manifest.json`.

## Examples

- `python3 evals/repair_benchmark/run.py --list-json --stage compile --limit 2`
- `python3 evals/repair_benchmark/run.py --mode quality --agent-count 4 --run-id local-quality`
- `python3 evals/repair_benchmark/run.py --mode agent-sweep --representative-count 10 --agent-count 1 --agent-count 2 --agent-count 4 --agent-count 8 --run-id local-sweep`

## Output Bundle

Each run writes `run-<id>/` with:

- `manifest.snapshot.json`
- `environment.json`
- `selection.json`
- `report.json`
- `summary.md`
- `cases/<case-id>/agents-<n>/result.json`
- captured `baseline`, `fix`, and `independent-rerun` logs for every attempted case

The aggregate report schema is `mercury-repair-benchmark-v1`.

# Mercury Repair Benchmark Runner

This harness turns the public eval corpus into a repair-quality benchmark by:

- reproducing the seeded failing verifier command in a disposable case copy
- running `mercury fix ... --noninteractive`
- reading the emitted `benchmark-run.json` contract from `.mercury/runs/`
- independently rerunning the verifier against the sandboxed `final-bundle`
- deriving false-greens from that independent rerun and aggregating accepted-patch, timing, and cost metrics under `mercury-repair-benchmark-v1`

Current publication scope is Rust-first. The default suite is `evals/v0/manifest.json`.

## Examples

- `python3 evals/repair_benchmark/run.py --list-json --stage compile --limit 2`
- `python3 evals/repair_benchmark/run.py --mode quality --agent-count 4 --run-id local-quality`
- `python3 evals/repair_benchmark/run.py --mode agent-sweep --representative-count 10 --agent-count 1 --agent-count 2 --agent-count 4 --agent-count 8 --run-id local-sweep`
- `python3 evals/repair_benchmark/run.py --mode quality --agent-count 4 --keep-workspaces --run-id debug-quality`
- `python3 evals/repair_benchmark/run.py --mode quality --agent-count 4 --run-id local-quality --resume`

## Output Bundle

Each run writes `run-<id>/` with:

- `manifest.snapshot.json`
- `environment.json`
- `selection.json`
- `report.json`
- `summary.md`
- `report.partial.json`
- `summary.partial.md`
- `cases/<case-id>/agents-<n>/result.json`
- captured `baseline`, `fix`, and `independent-rerun` logs for every attempted case

The aggregate report schema is `mercury-repair-benchmark-v1`.

Copied per-case workspaces are deleted by default after the independent rerun so larger corpus runs do not retain duplicate fixture trees. Pass `--keep-workspaces` when you need the copied workspace and `sandbox_run_root` metadata preserved for debugging.

Use `--resume` to continue an interrupted run from existing `cases/<case-id>/agents-<n>/result.json` outputs instead of rerunning completed attempts. The runner also refreshes `report.partial.json` and `summary.partial.md` after each attempted or resumed case so long runs always keep a current aggregate checkpoint.

## Public Report Publication

Convert raw benchmark runner aggregates into the checked-in public report surface with:

- `python3 evals/repair_benchmark/publish.py --pending` to regenerate the pending public scaffold without real metrics and clear any previously copied public `*.report.json` outputs
- `python3 evals/repair_benchmark/publish.py --quality-report <quality-report.json> --agent-sweep-report <agent-sweep-report.json>` to render completed benchmark aggregates into scrubbed public reports under `docs/benchmarks/`

Published stable targets:

- `docs/benchmarks/rust-v0-repair-benchmark.md`
- `docs/benchmarks/rust-v0-quality.report.json`
- `docs/benchmarks/rust-v0-agent-sweep.report.json`

Current checked-in public run ids:

- `20260311-quality`
- `20260311-agent-sweep`

The current checked-in Rust publication is intentionally narrow and honest: these runs recorded `0` accepted patches, `0` verified repairs, and `0` false-greens.

The public JSON surface preserves benchmark metrics and case outcomes while removing local run roots, copied candidate workspace paths, and API-key env metadata.

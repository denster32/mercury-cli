# Rust V0 Repair Benchmark

Status: pending first published secret-backed run.

This report is the checked-in publication target for the public Rust repair benchmark over `evals/v0`.

## Methodology

- Suite: `evals/v0/manifest.json`
- Runner: `python3 evals/repair_benchmark/run.py`
- Binary: `target/release/mercury-cli`
- Aggregate JSON schema: `mercury-repair-benchmark-v1`
- Quality mode: full selected Rust suite at a fixed `--max-agents` setting
- Agent-sweep mode: deterministic 10-case representative Rust subset at `--max-agents 1,2,4,8`
- Acceptance rule: non-empty accepted patch plus an independent verifier rerun against the sandboxed `final-bundle`
- False-green rule: Mercury marked the run verified, but the independent rerun failed or timed out

## Required Published Metrics

- attempted cases
- verified repair rate
- accepted patch rate
- false-green rate
- median time to first candidate
- median time to verified repair
- median and mean cost per attempted case
- median and mean cost per verified repair
- `--max-agents` speedup curve
- `--max-agents` cost curve

## Publication Notes

- The checked-in benchmark workflow now emits the machine-readable report and raw per-case logs needed to populate this file.
- This repository still needs one completed secret-backed run before this report can include real benchmark numbers.

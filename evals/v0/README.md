# Mercury CLI Eval Harness v0

This is a minimal v0.2 Rust eval harness for the trustworthiness gate.

Contents:
- `manifest.json`: canonical 10-case seeded Rust failure corpus
- `cases/*`: tiny standalone crates that should stay intentionally red in baseline mode
- `run.py`: baseline runner that executes selected cases and emits a report bundle
- `reports/.gitkeep`: checked-in scaffold for CI artifact output

Usage:
- `python3 evals/v0/run.py --list`
- `python3 evals/v0/run.py --case rust_type_mismatch --output-dir evals/v0/reports/local`
- `python3 evals/v0/run.py --output-dir evals/v0/reports/ci`

Run bundle layout:
- `run-<timestamp>/manifest.snapshot.json`
- `run-<timestamp>/environment.json`
- `run-<timestamp>/report.json`
- `run-<timestamp>/summary.md`
- `run-<timestamp>/cases/<case-id>/result.json`
- `run-<timestamp>/cases/<case-id>/stdout.txt`
- `run-<timestamp>/cases/<case-id>/stderr.txt`
- `_cargo-target/` for Cargo build output during the run

Current scope:
- baseline mode only
- validates that all 10 seeded cases fail in the expected way
- intended to wire into CI as a reproducible artifact producer

TODO:
- add repair-mode execution once the runtime can emit real plan/diff/verifier artifacts
- promote the report bundle into the main `mercury fix` artifact path when v0.2 runtime work lands

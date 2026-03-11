# Mercury CLI Eval Harness v1 TypeScript (v1.0 lane)

This harness is the TypeScript second-language eval lane for the current 1.0.0 branch contract.

Contents:
- `manifest.json`: canonical 50-case TypeScript baseline corpus contract
- `cases/*`: 10 canonical failing fixtures reused across 50 logical case ids
- `run.py`: manifest-driven baseline runner that validates expected-red command behavior and emits a report bundle
- `reports/.gitkeep`: checked-in scaffold for CI/local artifact output

Usage:
- `python3 evals/v1_typescript/run.py --list`
- `python3 evals/v1_typescript/run.py --list-json --stage lint --limit 2`
- `python3 evals/v1_typescript/run.py --tag language:typescript --limit 5`
- `python3 evals/v1_typescript/run.py --case ts_type_mismatch --output-dir evals/v1_typescript/reports/local`
- `python3 evals/v1_typescript/run.py --output-dir evals/v1_typescript/reports/ci`

Corpus shape:
- 50 manifest case ids across parse, compile, test, and lint failure stages
- stage distribution is fixed in v1 lane: parse=5, compile=20, test=15, lint=10
- 10 unique fixture directories under `cases/`
- each fixture family appears 5 times in the manifest: seed plus variants `v2` through `v5`
- only ids ending in an explicit `_v<digits>` suffix count as variants; ids with embedded `_v` elsewhere are treated as seed ids
- variants currently reuse the same fixture path and are differentiated by case id, provenance metadata, and tags for selection/reporting

Run bundle layout:
- `run-<timestamp>/manifest.snapshot.json`
- `run-<timestamp>/environment.json`
- `run-<timestamp>/selection.json`
- `run-<timestamp>/report.json`
- `run-<timestamp>/summary.md`
- `run-<timestamp>/cases/<case-id>/result.json`
- `run-<timestamp>/cases/<case-id>/stdout.txt`
- `run-<timestamp>/cases/<case-id>/stderr.txt`

Current scope:
- baseline mode only
- validates TypeScript fixture red-state contract and report reproducibility
- emits schema/version metadata for downstream CI checks
- intended as corpus/evidence infrastructure for TypeScript lane hardening

Known limits:
- this harness does not run the Mercury repair engine
- fixtures currently execute deterministic failing scripts; they do not install or run full `tsc`/eslint/jest toolchains
- passing harness runs do not prove end-to-end TypeScript repair success

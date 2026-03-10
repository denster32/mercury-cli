# Mercury CLI Eval Harness v0

This is the v0.3 Rust eval harness for the runtime trust gate.

Contents:
- `manifest.json`: canonical 50-case Rust eval corpus contract
- `cases/*`: 10 canonical red fixture crates reused across 50 logical case ids
- `run.py`: manifest-driven baseline runner that validates selected cases and emits a report bundle
- `reports/.gitkeep`: checked-in scaffold for CI artifact output

Usage:
- `python3 evals/v0/run.py --list`
- `python3 evals/v0/run.py --list-json --stage lint --limit 2`
- `python3 evals/v0/run.py --tag kind:variant --limit 5`
- `python3 evals/v0/run.py --case rust_type_mismatch --output-dir evals/v0/reports/local`
- `python3 evals/v0/run.py --case rust_type_mismatch_v5 --output-dir evals/v0/reports/local`
- `python3 evals/v0/run.py --output-dir evals/v0/reports/ci`

Corpus shape:
- 50 manifest case ids across parse, compile, test, and lint failure stages
- stage distribution is fixed in v0.3: parse=5, compile=20, test=15, lint=10
- 10 unique fixture directories under `cases/`
- each fixture family appears 5 times in the manifest: the seed case plus variants `v2` through `v5`
- only ids ending in an explicit `_v<digits>` suffix count as variants; ids with embedded `_v` elsewhere are treated as seed ids
- variants currently reuse the same on-disk crate path and are differentiated by case id, provenance metadata, and tags for selection/reporting

Run bundle layout:
- `run-<timestamp>/manifest.snapshot.json`
- `run-<timestamp>/environment.json`
- `run-<timestamp>/selection.json`
- `run-<timestamp>/report.json`
- `run-<timestamp>/summary.md`
- `run-<timestamp>/cases/<case-id>/result.json`
- `run-<timestamp>/cases/<case-id>/stdout.txt`
- `run-<timestamp>/cases/<case-id>/stderr.txt`
- `_cargo-target/` for Cargo build output during the run

Current scope:
- baseline mode only
- validates that selected manifest cases fail in the expected way
- emits a reproducible run bundle with manifest snapshot, selection metadata, report JSON, and per-case stdout/stderr
- report JSON includes manifest schema/version metadata so downstream CI checks can prove which corpus contract produced the run
- intended to wire into CI as a reproducible artifact producer for the v0.3 runtime path

CI draft-PR workflow contract:
- `.github/workflows/repair.yml` is the v0.3 CI auto-repair entrypoint and builds `mercury-cli` from source in the workflow run.
- supported workflow inputs are: `failure_command` (required), `repair_goal`, `source_ref`, `base_ref`, `setup_command`, `lint_command`, `max_agents`, `max_cost`, `artifact_retention_days`, and `dry_run`.
- workflow-call secrets accepted by contract: `INCEPTION_API_KEY`, `MERCURY_API_KEY`, and `inception_api_key` (provider-first fallback order resolves to `INCEPTION_API_KEY` in runtime env).
- the workflow always uploads an evidence bundle and validates required files before summary/PR steps (`summary.md`, `decision.json`, `environment.json`, `pr-body.md`, `repair.diff`, `repair.diffstat.txt`, baseline logs).
- a draft PR is only eligible when all verification gates pass: baseline reproduced, final bundle verified, patch applied, post-repair verifier exits `0`, and a non-empty non-`.mercury` diff exists.
- `dry_run` runs the same isolation + evidence flow but skips branch push and draft PR creation.
- workflow terminal statuses `verified_patch_ready` and `repair_not_verified` are non-blocking; `baseline_not_reproduced`, `missing_api_key`, `setup_failed`, and `internal_error` still publish artifacts but fail the workflow.

Notes:
- this harness validates the red-state corpus contract and artifact reproducibility
- it does not execute the full repair runtime; CI repair orchestration lives in `.github/workflows/repair.yml`
- cases are currently 50 logical ids backed by 10 physical fixture crates; promote to 50 physical fixtures only if needed for future benchmarking
- this harness should be treated as baseline-failure validation infrastructure, not as published repair-success benchmarking
- the TypeScript lane has a parallel baseline harness at `evals/v1_typescript/`

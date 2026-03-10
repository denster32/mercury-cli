# Mercury CLI Architecture (v0.3 Alpha)

This document describes the current runtime and trust boundaries for Mercury CLI as a v0.3 CI auto-repair alpha.

Mercury CLI is not a generic autonomous coding shell. The implemented product wedge is narrower:

- start from a failing Rust verifier command
- attempt bounded repair with Mercury models
- verify locally in isolation before acceptance
- emit a reviewable evidence bundle
- optionally open or update a draft PR through GitHub Actions

## Implemented Workflow Surfaces

### Local `fix`

`mercury-cli fix "<goal>"` runs a Rust-first repair loop with planning, candidate generation, verifier execution, and artifact emission under `.mercury/runs/<run-id>/`.

### Local `watch --repair`

`mercury-cli watch "<command>" --repair` auto-repair is intentionally limited to direct Rust verifier commands:

- `cargo test ...`
- `cargo check ...`
- `cargo clippy ...`
- optional env-prefix variants that still resolve directly to those commands

Composed shell commands (`&&`, pipes, redirection, shell wrappers like `make test` or `just test`) are observed but marked `repair_not_supported`.

### CI Draft PR Workflow

`.github/workflows/repair.yml` (`Mercury CI Auto-Repair Draft PR`) performs:

1. checkout and build `mercury-cli`
2. isolated baseline failure reproduction in a detached git worktree
3. Mercury repair attempt (`fix`)
4. post-repair verifier rerun
5. evidence bundle validation and upload
6. draft PR creation/update only when repair is verified, `dry_run != true`, and the workflow can push to the same repository with required permissions
7. final workflow status is blocking for orchestration failures (`baseline_not_reproduced`, `missing_api_key`, `setup_failed`, `internal_error`) even though artifacts are still uploaded

## Safety Model

The v0.3 alpha safety boundary is workflow-first and evidence-first.

### Candidate isolation

Repair attempts run in disposable worktrees (`.mercury/worktrees/` locally, detached worktree in CI workflow).

### Atomic accept/reject path

Rejected candidates are discarded with their sandbox. Accepted candidates are copied back after verification gates succeed.

### Verification gates before promotion

No patch is considered CI draft-PR eligible unless all are true:

- baseline failure reproduced
- run metadata indicates final bundle verification
- repair marked applied
- post-repair verifier exit is zero
- non-empty non-`.mercury` diff exists

### Reproducible artifacts

Runs are expected to emit inspectable evidence for replay and audit.

## Evidence Bundle Contract

The workflow validates a minimum artifact contract before summary publishing:

- `summary.md`
- `decision.json`
- `environment.json`
- `pr-body.md`
- `repair.diff`
- `repair.diffstat.txt`
- `logs/baseline.stdout.log`
- `logs/baseline.stderr.log`

When repair executes, bundle logs also include repair and post-repair verifier outputs; setup/init logs are included when those steps run.

If a nested Mercury run directory is available, it is copied into `mercury-run/` inside the uploaded bundle.

## Structured Data Boundaries

- Workflow decision/environment payloads are JSON with stable keys used by docs/tests.
- Eval harness (`evals/v0`) is manifest-driven and emits schema/version metadata in reports.
- Planner critique text remains advisory prose and should not be treated as a strict machine contract.

## Known v0.3 Alpha Limits

- Rust-first scope only for auto-repair targeting.
- `--max-agents` currently configures budget/scheduler surfaces; do not treat it as proven runtime swarm speedup.
- CI automation is draft-PR oriented, not autonomous merge.
- Public benchmark reporting is still behind corpus/harness readiness.

## Relationship to Case Studies

Reproducible operator flows are documented in:

- `docs/case-studies/local-red-to-green.md`
- `docs/case-studies/ci-draft-pr-flow.md`

Treat those files as the primary runbooks. This architecture document describes invariants and boundaries they rely on.

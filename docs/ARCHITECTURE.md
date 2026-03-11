# Mercury CLI Architecture (1.0.0-beta.1 Pre-Release Scope, Rust-First Repair + Scoped TypeScript Support)

This document describes the current runtime and trust boundaries for Mercury CLI in the `1.0.0-beta.1` pre-release branch scope, with repair quality centered on Rust, scoped selected TypeScript verifier-path support, and current observability and hardening surfaces.

Mercury CLI is not a generic autonomous coding shell. The implemented product wedge is narrower:

- start from a failing direct allowlisted verifier command, with Rust as the primary repair-quality target and selected TypeScript commands supported in scoped `fix` and CI flows
- attempt bounded repair with Mercury models
- verify locally in isolation before acceptance
- emit a reviewable evidence bundle
- optionally open or update a draft PR through GitHub Actions

## Implemented Workflow Surfaces

### Local `fix`

`mercury-cli fix "<goal>"` runs a repair loop with planning, candidate generation, verifier execution, and artifact emission under `.mercury/runs/<run-id>/`.

### Local `watch --repair`

`mercury-cli watch "<command>" --repair` auto-repair is intentionally limited to direct Rust verifier commands:

- `cargo test ...`
- `cargo check ...`
- `cargo clippy ...`
- optional env-prefix variants that still resolve directly to those commands

Composed shell commands (`&&`, pipes, redirection, shell wrappers like `make test` or `just test`) are rejected at watch command parsing time and do not start a watch cycle.

### Live Observability

`mercury-cli status --live` provides candidate-level runtime observability with configurable refresh (`--interval-ms`).

- In a TTY it renders the existing heatmap/agent/budget dashboard and appends a rolling live-event pane for candidate launches, status changes, phase activation, and runtime-state updates.
- When stdout is piped, it emits the same redacted runtime feed as JSONL so CI and local tooling can inspect candidate/phase/runtime events without scraping the terminal dashboard.

For CI-safe logs, `fix` and `watch` also support `--noninteractive`, and the CI workflow invokes `fix --noninteractive`.

### CI Draft PR Workflow

`.github/workflows/repair.yml` (`Mercury CI Auto-Repair Draft PR`) performs:

1. checkout and build `mercury-cli`
2. isolated baseline failure reproduction in a detached git worktree
3. Mercury repair attempt (`fix`)
4. post-repair verifier rerun
5. evidence bundle validation and upload
6. draft PR creation/update only when repair is verified, `dry_run != true`, and the workflow can push to the same repository with required permissions; verified reruns targeting the same base ref and failure command reuse the same repair branch/PR head
7. final workflow status is blocking for orchestration failures (`baseline_not_reproduced`, `missing_api_key`, `setup_failed`, `internal_error`) even though artifacts are still uploaded

## Safety Model

The `1.0.0-beta.1` pre-release safety boundary is workflow-first and evidence-first.

### Candidate isolation

Repair attempts run in disposable repo-copy/worktree isolation (`.mercury/worktrees/` locally, detached worktree in CI workflow). This is not a container/VM sandbox claim.

### Atomic accept/reject path

Rejected candidates are discarded with their isolated repo copy/worktree. Accepted candidates are copied back after verification gates succeed.

### Verification gates before promotion

No patch is considered CI draft-PR eligible unless all are true:

- baseline failure reproduced
- run metadata indicates final bundle verification
- repair marked applied
- post-repair verifier exit is zero
- non-empty non-`.mercury` diff exists

### Reproducible artifacts

Runs are expected to emit inspectable evidence for replay and audit for the execution path that actually ran.

Every run bundle now includes `audit.log` with JSONL event records (run start, plan readiness, execution result, completion, and watch-cycle milestones).

Runtime output written into artifacts is redacted for known API-key markers and configured API-key env names.

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
- Eval harnesses (`evals/v0` for Rust and `evals/v1_typescript` for TypeScript lane) are manifest-driven and emit schema/version metadata in reports.
- Planner critique text remains advisory prose and should not be treated as a strict machine contract.

## Enterprise Hardening Baselines

- Verifier allowlist enforces direct Rust cargo verifier commands and selected direct TypeScript verifier invocations by default (including supported env-prefix forms).
- Shell composition in verifier commands is blocked unless `MERCURY_ALLOW_UNSAFE_VERIFIER_COMMANDS=1` is set explicitly.
- Noninteractive mode is available for CI-oriented output surfaces.
- End-to-end `fix` and CI repair targeting support allowlisted Rust direct verifier commands plus scoped selected TypeScript direct verifier commands; local `watch --repair` remains Rust-only.

## Known 1.0.0-beta.1 Limits

- Local `watch --repair` remains Rust-only.
- `--max-agents` materially affects phased runtime dispatch and isolated candidate fanout. The repo now publishes scoped Rust benchmark speedup data under `docs/benchmarks/`, but it still does not claim broad overlapping-edit convergence from that setting.
- TypeScript support is intentionally scoped: selected direct verifier commands are supported in `fix`/CI flows, while watch-based auto-repair and broader command classes are still limited; this is not parity with the Rust repair surface.
- Live observability now exposes candidate/phase/runtime events, but it is still not a full conflict-telemetry or merge-decision explanation surface.
- CI automation is draft-PR oriented, not autonomous merge.
- Public benchmark reporting now exists under `docs/benchmarks/` for the selected Rust corpus, emitted by the dedicated repair benchmark workflow and publisher. Those checked-in numbers are bounded to the exact run ids and corpus selection documented there.
- TypeScript harness fixtures currently validate deterministic expected-red script outputs; this is useful for corpus/reporter contract checks but not a replacement for full benchmark-backed repair reporting.

## Relationship to Case Studies

Reproducible operator flows are documented in:

- `docs/case-studies/local-red-to-green.md`
- `docs/case-studies/ci-draft-pr-flow.md`

Treat those files as the primary runbooks. This architecture document describes invariants and boundaries they rely on.

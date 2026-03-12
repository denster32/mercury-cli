# Mercury CLI

Mercury CLI is a Rust-first Mercury-native CI auto-repair tool for teams using Inception Labs models.

The current branch is aligned to the Mercury CLI `1.0.0-beta.1` pre-release runtime surface. The real product wedge today is Rust-first repair: local `watch --repair` is for supported direct Rust verifier commands, while `fix` and the GitHub repair workflow are productized around direct allowlisted Rust verifier commands and the checked-in Tier 1 benchmark lane at `evals/v0/tier1-manifest.json`. TypeScript remains a frozen experimental lane for selected direct verifier commands; it currently relies on token-aware repository scanning and failure parsing rather than a real TypeScript parser, so it should not be read as parity with Rust repair quality. Artifact bundles, phased candidate fanout via `fix --max-agents N`, `status --live` runtime events, verifier allowlisting, audit logs, and output redaction are implemented. Candidate verification isolation is repo-copy/worktree based under `.mercury/worktrees/` (not a stronger process/container sandbox claim). `.github/workflows/repair.yml` only opens or updates draft PRs for verified repairs when `dry_run=false` and same-repo write permissions are available.

## Install

### From source

```bash
git clone https://github.com/denster32/mercury-cli
cd mercury-cli
cargo build --release
```

Run commands with either:

- `./target/release/mercury-cli ...` (no global install)
- `cargo install --path .` then `mercury-cli ...`

### From GitHub Releases

Official release archives are currently published only for macOS arm64 and Linux x86_64. Hyphenated versions such as `v1.0.0-beta.1` publish as GitHub prereleases for that exact branch surface and should not be read as a broader platform-support contract; plain `v1.0.0` remains reserved for the stable GA release line. Windows is covered by the CI test matrix, but there is no official Windows release archive in the current repo. Use a source build when you need unreleased branch-head behavior or a platform outside the current release matrix.

### API key

`INCEPTION_API_KEY` is the preferred environment variable. `MERCURY_API_KEY` still works as a backward-compatible fallback.

```bash
export INCEPTION_API_KEY="your-api-key"
# Optional fallback for older configs or older CLI help text:
export MERCURY_API_KEY="$INCEPTION_API_KEY"
```

## 60-Second Demo

The fastest real path today is local Rust repair: reproduce a failing verifier command, run `watch --repair`, inspect the artifact bundle, and keep only verified changes.

```bash
git clone https://github.com/denster32/mercury-cli
cd mercury-cli
cargo build --release

export INCEPTION_API_KEY="your-api-key"
export MERCURY_API_KEY="$INCEPTION_API_KEY"

./target/release/mercury-cli init
./target/release/mercury-cli watch "cargo test -p your-crate" --repair
```

What a good current run should leave behind:

- a final watch decision printed to the terminal
- an artifact bundle path under `.mercury/runs/`
- `watch.json` plus the watched command's stdout and stderr
- copied repair artifacts when Mercury applies a fix attempt
- no partial writes from rejected candidates

Important limits:

- local `watch --repair` auto-repair is currently Rust-only and targeted at direct `cargo` verifier commands: `cargo test`, `cargo check`, and `cargo clippy`
- optional env-prefix forms are supported when they still resolve directly to those commands (for example `RUST_BACKTRACE=1 cargo test --quiet`)
- non-allowlisted watch commands are rejected before execution and before any watch-cycle artifacts are created
- `watch` without `--repair` is report-only for allowlisted verifier commands

## Case Studies

Reproducible repo-backed walkthroughs:

- [Local red -> green watch-repair flow](docs/case-studies/local-red-to-green.md)
- [CI-oriented repair to draft PR flow](docs/case-studies/ci-draft-pr-flow.md)

## Capability Matrix

Examples below assume you built from source in this repo, so command invocations use `./target/release/mercury-cli`. If you installed via `cargo install --path .`, replace with `mercury-cli`.

| Surface | Status | Reality in the current repo |
| --- | --- | --- |
| `./target/release/mercury-cli init` | Available | Creates `.mercury/` config and thermal database. |
| `./target/release/mercury-cli plan <goal>` | Available | Produces a structured repair plan and thermal assessments. |
| `./target/release/mercury-cli ask <query>` | Available | Repo-aware Mercury 2 Q&A. |
| `./target/release/mercury-cli status [--heatmap] [--agents] [--budget]` | Available | Reports thermal state and scheduler metadata. |
| `./target/release/mercury-cli status --live [--interval-ms N]` | Available | Streams candidate, phase, and runtime events in a TTY dashboard and emits JSONL event records when piped. |
| `./target/release/mercury-cli edit apply` | Available | Concrete Mercury Edit apply surface for replacement snippets or patch content. It is not an instruction-driven repair endpoint. |
| `./target/release/mercury-cli edit complete` | Available | Completion-style Mercury Edit request for a file or cursor location. |
| `./target/release/mercury-cli edit next` | Available | Next-edit prediction using current file state plus focused cursor and recent-snippet context. |
| `./target/release/mercury-cli fix <description>` | Available | Repair flow with planning, candidate generation, isolated repo-copy/worktree verification, and artifacts for direct allowlisted Rust verifier commands aligned with the Tier 1 Rust beta lane in `docs/benchmarks/`. It also includes a frozen experimental TypeScript lane for selected direct verifier commands, but that lane is not parser-backed parity with Rust. |
| `./target/release/mercury-cli fix <description> --noninteractive` | Available | CI-safe output mode for log parsing and deterministic summary lines. |
| `./target/release/mercury-cli watch <direct allowlisted verifier command>` | Available | Re-runs an allowlisted direct verifier command when repo contents change and records a watch artifact bundle per cycle. |
| `./target/release/mercury-cli watch <direct Rust verifier command> --repair` | Available with limits | End-to-end local repair loop for direct `cargo test`, `cargo check`, or `cargo clippy` commands (including env-prefix variants), with targeted verifier reuse and post-repair confirmation. |
| `./target/release/mercury-cli watch <command> --noninteractive` | Available | CI-safe watch output mode with compact cycle decisions. |
| `./target/release/mercury-cli watch <composed shell command> --repair` | Not supported | Commands with pipelines or shell chaining are rejected by the watch command allowlist before cycle execution. |
| `./target/release/mercury-cli config get` / `validate` | Available | Reads or validates config values. |
| `./target/release/mercury-cli config set` | Available with limits | Safely updates the documented scalar keys in `.mercury/config.toml` and validates the full config before write. Unsupported keys still require direct TOML editing. |
| Manual CI-to-draft-PR handoff | Documented workflow | The repo includes a case study for publishing artifacts and opening a draft PR after a verified local or CI reproduction. |
| GitHub Action repair workflow | Available with limits | The `Mercury CI Auto-Repair Draft PR` workflow in `.github/workflows/repair.yml` reproduces a failure in isolation, runs Mercury repair for direct allowlisted Rust verifier commands and the frozen experimental TypeScript verifier lane when baseline is red and an API key is present, uploads artifacts for every terminal status, and opens or updates a draft PR only when repair is verified, `dry_run=false`, and the workflow can push to the same repository. Verified reruns targeting the same base ref and failure command reuse the same repair branch/PR head instead of minting a new branch name per run. Use `dry_run` when you want the evidence bundle without branch or PR mutation. |
| Eval corpus | Available | `evals/v0/manifest.json` is the 50-case Rust baseline corpus, `evals/v0/tier1-manifest.json` is the 35-case Tier 1 Rust repair beta lane, and `evals/v1_typescript/manifest.json` is the 50-case frozen experimental TypeScript baseline harness. The TypeScript corpus is baseline coverage, not parser-backed repair-parity evidence. |
| Published repair benchmark report | Available with scoped evidence | `docs/benchmarks/rust-v0-repair-benchmark.md`, `docs/benchmarks/rust-v0-quality.report.json`, and `docs/benchmarks/rust-v0-agent-sweep.report.json` are generated by `evals/repair_benchmark/publish.py` from aggregate runner outputs. Those checked-in numbers are the product truth for the Tier 1 Rust beta lane at `evals/v0/tier1-manifest.json`, including repair outcome distribution and execution diagnostics for misses. |
| `./target/release/mercury-cli fix --max-agents N` | Available with scoped benchmark evidence | Materially changes phased runtime dispatch with real parallel candidate execution and isolated candidate fanout. `docs/benchmarks/` publishes representative runtime and cost curves for `--max-agents 1,2,4,8` on the Tier 1 Rust beta lane, but those exact runs should not be treated as a broad convergence or repair-quality claim beyond the checked-in corpus and run ids. |
| Generic workflow DSL / `agent run` | Out of scope for tagged 1.0.0 GA | Intentionally deferred until the repair workflow is stronger. |

## Safety Model

Mercury CLI should be evaluated like a repair system, not a chatbot shell.

### Candidate isolation

Repair candidates are generated and verified in disposable repo-copy/worktree isolation under `.mercury/worktrees/` instead of mutating the user worktree directly. This is filesystem/worktree isolation, not a process/container sandbox guarantee.

### Atomic acceptance

Rejected candidates are discarded with the workspace. Accepted candidates are copied back only after local verification succeeds.

### Local verification first

Parse, test, and lint commands are local gates. Model output does not become an accepted repository change until those gates pass.

### Verifier allowlist boundary

By default, repair verifier commands must resolve to direct allowlisted Rust or selected direct TypeScript verifier invocations (including supported env-prefix variants) without shell composition. End-to-end `fix` and CI repair flows support those allowlisted commands, but the TypeScript path remains a frozen experimental lane and narrower than the Rust lane; local `watch --repair` targeting remains Rust-only today. Shell composition is rejected unless `MERCURY_ALLOW_UNSAFE_VERIFIER_COMMANDS=1` is explicitly set.

### Structured output boundary

Planner and eval artifacts use strict JSON schemas where implemented in the runtime and harness. Critique output is still best-effort prose from Mercury 2, so it should be treated as advisory context rather than a schema-validated contract.

### Redaction and audit trail

Run output is redacted for known API key marker lines and configured API-key env names before writing artifacts or replaying command logs. Every `fix` run and every `watch` cycle writes append-only audit events to `audit.log` in the run bundle.

### Reproducible evidence

Repair runs are expected to leave behind an artifact bundle under `.mercury/runs/` with plan/candidate/verifier/timing/cost evidence for the path executed. `watch --repair` adds a watch-level record for the watched command and confirmation rerun when repair executes.

## Artifact Bundle

A successful watch-repair cycle in the current `1.0.0-beta.1` pre-release runtime should leave enough evidence to replay the decision:

- `watch.json` with the watched command, decision, timestamps, and repair record
- `initial.stdout.txt` and `initial.stderr.txt` from the failing command
- `initial.failure.json` when a structured failure parse is available for the initial command result
- `confirmation.stdout.txt` and `confirmation.stderr.txt` when Mercury reruns the verifier after repair
- `confirmation.failure.json` when a structured failure parse is available for the confirmation rerun
- `audit.log` with JSONL audit events for run start/plan/execution/decision
- mirrored nested repair artifacts when the fix flow ran: `repair/diff.patch`, `repair/execution-summary.json`, `repair/final-verification.json`, `repair/metadata.json`, plus `repair/plan.json` and `repair/grounded-context.json` when present in the source `fix` run bundle
- the source `fix` artifact root recorded in `watch.json` when you need the full nested repair bundle

For direct `./target/release/mercury-cli fix` runs, the run bundle also includes:

- `plan.json` and `assessments.json`
- `execution-summary.json` and `final-verification.json` when final verification ran
- `agent-logs.json` and `thermal-aggregates.json`
- `metadata.json`
- `audit.log`
- `diff.patch` when an accepted candidate produced a final patch
- `swarm-state.json` when runtime state was captured

For the `Mercury CI Auto-Repair Draft PR` workflow, the uploaded evidence bundle includes:

- `summary.md`, `decision.json`, `environment.json`, and `pr-body.md`
- `repair.diff`, `repair.diffstat.txt`, and `git-status.txt`
- `logs/baseline.stdout.log` and `logs/baseline.stderr.log`
- `logs/repair.stdout.log`, `logs/repair.stderr.log`, `logs/post-repair.stdout.log`, and `logs/post-repair.stderr.log` when a repair attempt ran
- `logs/setup.stdout.log` and `logs/setup.stderr.log` when `setup_command` was used
- `logs/mercury-init.stdout.log` and `logs/mercury-init.stderr.log` when workflow init was run
- copied `mercury-run/` artifacts when the workflow captured a nested `fix` run
- `internal-error.txt` when orchestration hits an unexpected internal failure

Minimum required by workflow contract before summary publishing:

- `summary.md`
- `decision.json`
- `environment.json`
- `pr-body.md`
- `repair.diff`
- `repair.diffstat.txt`
- `logs/baseline.stdout.log`
- `logs/baseline.stderr.log`

Workflow status behavior:

- `verified_patch_ready` and `repair_not_verified` are non-blocking terminal statuses
- `baseline_not_reproduced`, `missing_api_key`, `setup_failed`, and `internal_error` still upload the evidence bundle but end the workflow as failed

## Eval Corpus

The repo includes three manifest-driven eval assets:

- `evals/v0/manifest.json`: 50-case Rust baseline corpus (`rust-v0.3-seeded`)
- `evals/v0/tier1-manifest.json`: 35-case Tier 1 Rust repair beta lane (`rust-v0.3-tier1`)
- `evals/v1_typescript/manifest.json`: 50-case frozen experimental TypeScript corpus (`typescript-v1.0-seeded`)

What that means today:

- the Rust baseline and TypeScript harnesses exercise reproducible red-state checks (`evals/v0/run.py`, `evals/v1_typescript/run.py`)
- the Tier 1 manifest narrows public Rust repair claims to solvable compile, test, and lint failures
- the repo has the raw ingredients for a benchmark loop that can explain misses, not just total outcomes

What it does not mean yet:

- the checked-in Rust benchmark reports under `docs/benchmarks/` are intentionally narrow: they cover the Tier 1 Rust beta manifest, the exact run ids listed there, and the execution diagnostics emitted for those misses, not a universal repair-quality claim
- TypeScript harness pass/fail proves baseline fixture contract only; it is supportive evidence for a frozen experimental lane built on token-aware scanning and failure parsing, not a standalone end-to-end TypeScript repair quality benchmark
- you should treat these corpora as evaluation scaffolding, not finished market-grade benchmark reporting

## Versioning and Migration Notes

- The current branch is aligned to `1.0.0-beta.1`. Matching hyphenated tags publish GitHub prerelease binaries for that exact runtime surface and should not be read as a broader support commitment; plain `v1.0.0` remains reserved for the stable GA release line.
- Official release archives are currently limited to macOS arm64 and Linux x86_64. Prefer matching release assets when a tag exists for the runtime you want; use source installs for unreleased branch-head behavior or platforms outside that matrix.
- `INCEPTION_API_KEY` is provider-preferred; `MERCURY_API_KEY` remains backward-compatible fallback.
- TypeScript support currently includes corpus coverage, token-aware repo mapping and symbol extraction, failure classification, and selected direct verifier-command support in `fix` and CI repair paths. It remains a frozen experimental scoped lane, not a real-parser-backed peer to Rust repair quality, and `watch --repair` remains Rust-only.

## What the Current Runtime Supports

- Rust-first repair workflows, with the most operator-ready path being local Rust `watch --repair` and Rust-first `fix`/CI verifier flows
- local `watch --repair` for supported direct Rust verifier commands
- phased execution with isolated candidate workspaces and `--max-agents`-driven fanout
- artifact bundles for watch cycles and fix runs
- local verification before acceptance
- Mercury 2 for planning and critique
- Mercury Edit for focused edits
- `status --live` candidate, phase, and runtime observability via TTY pane or JSONL stream
- verifier allowlisting, output redaction, and append-only audit logs
- frozen experimental TypeScript support for token-aware repo mapping/symbol extraction, failure parsing, and selected direct verifier commands in `fix` and CI repair paths
- official release archives for macOS arm64 and Linux x86_64
- manual or workflow-driven promotion of a verified run into a draft PR
- checked-in Rust benchmark evidence under `docs/benchmarks/` with scrubbed machine-readable aggregates, repair outcome distribution, execution diagnostics, and published `--max-agents` curves for the current Tier 1 corpus
- documented limits for incomplete surfaces

## Preview and Roadmap

### Preview in the current repo

- broader watch auto-repair coverage outside direct Rust verifier commands

### Roadmap after the current runtime

- broader CI repair automation beyond the current workflow-dispatch draft-PR path
- broader conflict arbitration for overlapping edits beyond the current narrow runtime suppression path
- TypeScript expansion beyond the current frozen experimental selected direct verifier-command support stays gated behind stronger Rust benchmark outcomes
- benchmark expansion beyond the current Tier 1 Rust beta corpus, run ids, and methodology envelope published under `docs/benchmarks/`
- richer live observability around conflict alerts, winner selection, and phase-routing telemetry beyond the current event stream

TypeScript note: token-aware repository mapping/symbol extraction, failure parsing, and selected direct verifier-command support are implemented in the current branch for `fix` and CI repair flows as a frozen experimental scoped lane. The repo does not currently ship a real TypeScript parser, so this should not be read as parity with Rust repair quality. Local `watch --repair` remains intentionally Rust-only.

## What the Current Repo Does Not Claim

- benchmark-backed `--max-agents` results beyond the exact Rust corpus and run ids published under `docs/benchmarks/`
- broad overlapping-edit convergence across arbitrarily many concurrent candidates
- broad language support beyond the current Rust-first repair surface and frozen experimental TypeScript support
- official release binaries beyond macOS arm64 and Linux x86_64
- zero-touch autonomous repair for every failing repo

## Configuration

Example `.mercury/config.toml`:

```toml
[api]
mercury2_endpoint = "https://api.inceptionlabs.ai/v1/chat/completions"
mercury_edit_endpoint = "https://api.inceptionlabs.ai/v1"
api_key_env = "INCEPTION_API_KEY"

[scheduler]
max_concurrency = 20
max_cost_per_command = 0.50
max_agents_per_command = 100
retry_limit = 3
backoff_base_ms = 500

[verification]
parse_before_write = true
test_after_write = true
lint_after_write = true
mercury2_critique_on_failure = true
test_command = "cargo test"
lint_command = "cargo clippy"
```

Compatibility note: older configs may still reference `MERCURY_API_KEY`. Current docs prefer `INCEPTION_API_KEY` and treat the older name as a fallback.

## Architecture

Mercury CLI has four practical layers:

1. `Planner`: Mercury 2 turns a goal plus repository context into a bounded repair plan.
2. `Edit engine`: Mercury Edit produces focused mutations and next-edit suggestions.
3. `Verifier`: local parse, test, and lint commands decide whether a candidate is acceptable.
4. `Runtime`: disposable workspaces, artifacts, cost tracking, and acceptance rules keep the workflow reproducible.

Implementation detail, trust boundaries, and roadmap notes live in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Development

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --verbose
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and release guidance.

## Security

See [SECURITY.md](SECURITY.md) for vulnerability reporting guidance.

## License

Mercury CLI is source-available under a custom non-commercial license.

- You can use, modify, and share it for personal, educational, research, evaluation, and other non-commercial purposes.
- You cannot profit from it, use it in commercial operations, or deploy derivative works commercially without Dennis Palucki's prior written permission.
- Commercial terms are handled separately and may include revenue sharing or other negotiated terms.

See [LICENSE](LICENSE) for the binding terms and [COMMERCIAL_LICENSE.md](COMMERCIAL_LICENSE.md) for the plain-English summary and contact path.

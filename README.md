# Mercury CLI

Mercury CLI is Mercury-native CI auto-repair for Rust-first teams using Inception Labs models.

The current repo state is best described as an honest v0.3 alpha: local `watch --repair` works for supported Rust verifier commands, artifact bundles are real, the repo includes a 50-case Rust eval corpus, and `.github/workflows/repair.yml` can open or update draft PRs only for verified repairs when `dry_run=false` and same-repo write permissions are available. Mercury CLI is not yet a generic agent shell or a proven concurrent swarm runtime.

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

The repo includes release workflows for macOS and Linux archives. Use source builds until you have a tagged release that matches the repo state you want to run.

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

What a good v0.3 run should leave behind:

- a final watch decision printed to the terminal
- an artifact bundle path under `.mercury/runs/`
- `watch.json` plus the watched command's stdout and stderr
- copied repair artifacts when Mercury applies a fix attempt
- no partial writes from rejected candidates

Important limits:

- auto-repair is currently Rust-only and targeted at direct `cargo` verifier commands: `cargo test`, `cargo check`, and `cargo clippy`
- optional env-prefix forms are supported when they still resolve directly to those commands (for example `RUST_BACKTRACE=1 cargo test --quiet`)
- shell composition like `cargo test && cargo clippy` or `cargo test | tee out.txt` is watched, but not auto-repaired
- `watch` without `--repair` is report-only by default

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
| `./target/release/mercury-cli edit apply` | Available | Apply-style Mercury Edit request for focused mutation. |
| `./target/release/mercury-cli edit complete` | Available | Completion-style Mercury Edit request for a file or cursor location. |
| `./target/release/mercury-cli edit next` | Available | Next-edit prediction using current file state and edit history. |
| `./target/release/mercury-cli fix <description>` | Available | Rust-first repair flow with planning, candidate generation, sandbox verification, and artifacts. |
| `./target/release/mercury-cli watch <command>` | Available | Re-runs a shell command when repo contents change and records a watch artifact bundle per cycle. |
| `./target/release/mercury-cli watch <direct Rust verifier command> --repair` | Available with limits | End-to-end local repair loop for direct `cargo test`, `cargo check`, or `cargo clippy` commands (including env-prefix variants), with targeted verifier reuse and post-repair confirmation. |
| `./target/release/mercury-cli watch <composed shell command> --repair` | Not supported | Commands with pipelines or shell chaining are watched, but auto-repair is intentionally refused. |
| `./target/release/mercury-cli config get` / `validate` | Available | Reads or validates config values. |
| `./target/release/mercury-cli config set` | Preview | CLI surface exists, but direct TOML editing may still be the more dependable path. |
| Manual CI-to-draft-PR handoff | Documented workflow | The repo includes a case study for publishing artifacts and opening a draft PR after a verified local or CI reproduction. |
| GitHub Action repair workflow | Available with limits | The `Mercury CI Auto-Repair Draft PR` workflow in `.github/workflows/repair.yml` reproduces a failure in isolation, runs Mercury repair when baseline is red and an API key is present, uploads artifacts for every terminal status, and opens or updates a draft PR only when repair is verified, `dry_run=false`, and the workflow can push to the same repository. Use `dry_run` when you want the evidence bundle without branch or PR mutation. |
| Eval corpus | Available | `evals/v0/manifest.json` currently contains 50 Rust cases used by the manifest-driven harness. |
| Published repair benchmark report | Not yet | The corpus exists, but the repo does not yet publish a full public repair benchmark with accepted-patch and false-green claims. |
| `./target/release/mercury-cli fix --max-agents N` | Available with limits | Changes scheduler and budget settings today. Do not market it as proof of real concurrent swarm speedup until the executor is parallel in runtime and benchmarked. |
| Generic workflow DSL / `agent run` | Not planned before v1.0 | Intentionally deferred until the repair workflow is stronger. |

## Safety Model

Mercury CLI should be evaluated like a repair system, not a chatbot shell.

### Candidate isolation

Repair candidates are generated and verified in disposable workspaces under `.mercury/worktrees/` instead of mutating the user worktree directly.

### Atomic acceptance

Rejected candidates are discarded with the workspace. Accepted candidates are copied back only after local verification succeeds.

### Local verification first

Parse, test, and lint commands are local gates. Model output does not become an accepted repository change until those gates pass.

### Structured output boundary

Planner and eval artifacts use strict JSON schemas where implemented in the runtime and harness. Critique output is still best-effort prose from Mercury 2, so it should be treated as advisory context rather than a schema-validated contract.

### Reproducible evidence

Each repair run should leave behind an artifact bundle under `.mercury/runs/` with the plan, candidate diffs, verifier output, timing, and cost metadata. `watch --repair` adds a watch-level record for the watched command and confirmation rerun.

## Artifact Bundle

A successful v0.3 watch-repair cycle should leave enough evidence to replay the decision:

- `watch.json` with the watched command, decision, timestamps, and repair record
- `initial.stdout.txt` and `initial.stderr.txt` from the failing command
- `confirmation.stdout.txt` and `confirmation.stderr.txt` when Mercury reruns the verifier after repair
- mirrored nested repair artifacts when the fix flow ran: `repair/diff.patch`, `repair/execution-summary.json`, `repair/final-verification.json`, and `repair/metadata.json`
- the source `fix` artifact root recorded in `watch.json` when you need the full nested repair bundle

For direct `./target/release/mercury-cli fix` runs, the run bundle also includes:

- `plan.json` and `assessments.json`
- `execution-summary.json` and `final-verification.json` when final verification ran
- `agent-logs.json` and `thermal-aggregates.json`
- `metadata.json`
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

The repo includes a 50-case Rust eval corpus at `evals/v0/manifest.json`.

What that means today:

- the manifest exists and is large enough to exercise the current Rust-first workflow
- the harness is manifest-driven and reproducible
- the repo has the raw ingredients for a stronger benchmark loop

What it does not mean yet:

- the repo does not yet publish a repair benchmark report with accepted-patch rate or false-green claims
- you should treat the corpus as proof of evaluation scaffolding, not finished market-grade benchmark reporting

## What v0.3 Supports

- Rust-first repair workflows
- local `watch --repair` for supported direct Rust verifier commands
- artifact bundles for watch cycles and fix runs
- local verification before acceptance
- Mercury 2 for planning and critique
- Mercury Edit for focused edits
- manual or workflow-driven promotion of a verified run into a draft PR
- documented limits for incomplete surfaces

## Preview and Roadmap

### Preview in the current repo

- `config set` as a fully dependable config-editing surface
- source-vs-release install alignment until tagged binaries are routine
- broader watch auto-repair coverage outside direct Rust verifier commands

### Roadmap after the current v0.3 alpha

- broader CI repair automation beyond the current workflow-dispatch draft-PR path
- true multi-worktree concurrent candidate execution
- conflict arbitration across overlapping edits
- a second supported language after the Rust-first path
- public benchmark reporting for repair outcomes, not only corpus manifests
- live observability for active candidates, cost, and acceptance decisions

## What the Current Repo Does Not Claim

- real swarm speedup from `--max-agents`
- safe parallel candidate execution across many worktrees
- broad language support beyond the Rust-first path
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

MIT

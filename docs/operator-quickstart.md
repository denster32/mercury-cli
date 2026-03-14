# Mercury CLI Operator Quickstart

This page is the fastest way to choose the right Mercury repair flow and interpret the output you get back.

Current product truth: Mercury CLI should be treated as a Rust direct cargo verifier repair beta. The operator-ready path is direct Rust verifier repair around `cargo test`, `cargo check`, and `cargo clippy`, with local `watch --repair` for fast iteration and the GitHub workflow for artifacted CI repair plus optional draft-PR promotion.

## Choose the Flow

Use local `watch --repair` when:

- you already have a failing Rust verifier command on your machine
- you want the shortest red to green loop
- you want to inspect the accepted diff before any branch or PR mutation

Use the CI workflow when:

- you need an uploaded artifact bundle for review or handoff
- you want isolated reruns on GitHub-hosted runners
- you want Mercury to open or update a draft PR after a verified repair

If you are unsure, start local. Move to CI after you can reproduce the same direct verifier command reliably.

If you want a disposable demo first, start with [starter-repos/README.md](../starter-repos/README.md). Those starter repos are the canonical onboarding path before you point Mercury at a real codebase.

## Local Watch-Repair

Supported local auto-repair target surface:

- `cargo test ...`
- `cargo check ...`
- `cargo clippy ...`
- env-prefix variants that still resolve directly to those commands

Minimal local flow:

```bash
cargo build --release
export INCEPTION_API_KEY="<your-key>"
export MERCURY_API_KEY="$INCEPTION_API_KEY"

./target/release/mercury-cli init
./target/release/mercury-cli watch "cargo test -p your-crate" --repair
```

What to expect:

- Mercury reruns the watched verifier command on startup and on repo changes
- supported red states can trigger a bounded repair attempt
- candidate verification runs in isolated repo-copy/worktree paths under `.mercury/worktrees/`
- only accepted edits are copied back into your worktree
- every cycle writes an artifact bundle under `.mercury/runs/`

Use the full walkthrough at [Local red -> green watch-repair flow](case-studies/local-red-to-green.md) when you want the exact operator steps.

## CI Draft-PR Flow

Use the in-repo workflow when you want the auditable CI path:

```bash
gh workflow run "Mercury CI Auto-Repair Draft PR" \
  -f failure_command='cargo test --all-features --verbose' \
  -f repair_goal='Repair the failing CI verifier and keep the change reviewable'
```

What to expect:

- baseline failure reproduction happens before repair
- the workflow uploads artifacts for every terminal state
- branch push and draft-PR mutation happen only after a verified repair, a non-empty diff, `dry_run=false`, and same-repo write permissions
- `dry_run=true` is review-only and still publishes the artifact bundle

Use the full walkthrough at [CI-oriented repair to draft PR flow](case-studies/ci-draft-pr-flow.md) when you need the full workflow-input and promotion details.

## Read the Artifact Bundle

Treat the artifact bundle as the source of truth for what happened.

For local `watch --repair`, start with:

- `summary-index.json`: top-level decision, headline, failure reason rollup, candidate lineage, winning candidates when present, and direct links to the relevant nested artifacts
- `watch.json`: watched command, cycle decision, timestamps, and nested repair reference
- `initial.stdout.txt` and `initial.stderr.txt`: baseline command output
- `confirmation.stdout.txt` and `confirmation.stderr.txt`: post-repair verifier output when repair ran
- `audit.log`: append-only event stream for the cycle
- `repair/`: nested repair evidence when Mercury invoked the fix flow

For CI workflow runs, start with:

- `artifact-index.json`: stable top-level index for the CI bundle, including the one-screen summary entrypoint and the required artifact contract
- `summary.md`: human-readable run summary with the nested Mercury run headline, failure reason rollup, candidate lineage, and winning candidate summary when a nested repair bundle was captured
- `decision.json`: machine-readable terminal decision, PR eligibility, and nested Mercury run highlights under `mercury_run`
- `environment.json`: run metadata, refs, and workflow context
- `repair.diff` and `repair.diffstat.txt`: accepted patch shape when present
- `logs/`: baseline, repair, post-repair, setup, and init logs when those steps ran
- `mercury-run/`: nested fix artifacts when the workflow captured the Mercury run directory

If you only have time for one check, open `summary-index.json` for local runs or `artifact-index.json` for CI runs first. From the CI index, jump to `summary.md`, then confirm the verifier rerun output and the diff.

## Interpret Statuses

Local watch-cycle outcomes:

- `repaired_and_verified`: Mercury applied a patch and the watched verifier reran green
- `repair_applied_but_command_still_failing`: Mercury copied back a patch, but the watched verifier still failed on confirmation
- `repair_not_applied`: no candidate survived verification strongly enough to be promoted
- `repair_flow_failed`: the nested repair run failed before it could complete normally
- `repair_not_supported`: the watched command shape is outside the current local auto-repair allowlist

CI workflow outcomes:

- `verified_patch_ready`: the workflow reproduced baseline failure, verified the repair, and produced a PR-ready patch
- `repair_not_verified`: Mercury attempted repair, but the final bundle did not clear the verification gate for promotion
- `baseline_not_reproduced`, `missing_api_key`, `setup_failed`, `internal_error`: workflow-level blockers that still upload artifacts for diagnosis

Operator rule:

- `verified` means the relevant verifier reran green after the accepted patch in the isolated repair path
- `repair_not_verified` means you still got evidence, but the run did not earn promotion into a trusted patch outcome

## Use `status --live`

`mercury-cli status --live` is the current runtime-event surface. Use it to watch candidate launches, status changes, phase activation, runtime updates, and candidate win/loss/suppression explanations while a repair run is active.

Current limit: it now explains candidate outcomes from persisted runtime metadata, but the artifact bundle remains the source of truth for final diff review, verifier output, and promotion decisions.

## Next Docs

- [Starter repos](../starter-repos/README.md)
- [Local Rust watch-repair starter repo](../starter-repos/local-rust-watch-repair/README.md)
- [CI draft-PR repair starter repo](../starter-repos/ci-draft-pr-repair/README.md)
- [Supported Rust verifier classes](supported-rust-verifier-classes.md)
- [Known limitations](known-limitations.md)
- [Diligence pack](diligence-pack.md)
- [Local red -> green watch-repair flow](case-studies/local-red-to-green.md)
- [CI-oriented repair to draft PR flow](case-studies/ci-draft-pr-flow.md)
- [Benchmarks overview](benchmarks/README.md)

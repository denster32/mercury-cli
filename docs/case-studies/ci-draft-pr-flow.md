# Case Study: CI Repair Evidence Bundle and Conditional Draft PR Flow

This walkthrough describes the CI-oriented repair flow supported by the current repo.

The repo includes the `Mercury CI Auto-Repair Draft PR` workflow in `.github/workflows/repair.yml`. It reproduces a failing verifier command in isolation, runs Mercury repair, and uploads artifacts for every terminal state. It only attempts branch push and draft-PR mutation when the repair is verified, `dry_run=false`, and the workflow can push back to the same repository.

## Goal

Reproduce failure in CI conditions, run isolated Mercury repair, verify locally, publish artifacts, and optionally let the workflow promote a verified patch into a draft PR carrying evidence.

## Preconditions

- branch with a reproducible failing verifier command
- repository or organization secret configured for repair jobs as `INCEPTION_API_KEY`, `MERCURY_API_KEY`, or `inception_api_key`
- same-repository workflow context with `contents: write` and `pull-requests: write` so the workflow can push a repair branch and mutate a PR when eligible
- if that write path is unavailable, plan to use `dry_run=true` and perform the PR handoff manually
- workflow name is exactly `Mercury CI Auto-Repair Draft PR` from `.github/workflows/repair.yml`

## Step 1: Reproduce Failure in CI

Use the repo workflow baseline to confirm red status.

```bash
git push origin <branch-with-failure>
# Observe the failing verifier command in your branch checks
```

Record:
- failing job name
- failing command
- first failing file/symbol/error class

## Step 2: Run Isolated Repair Attempt

Use the in-tree workflow when you want the automated draft-PR path:

```bash
gh workflow run "Mercury CI Auto-Repair Draft PR" \
  -f failure_command='cargo test --all-features --verbose' \
  -f repair_goal='Repair the failing CI verifier and keep the change reviewable'
```

The workflow is currently manual (`workflow_dispatch`) or reusable (`workflow_call`); it is not wired as an automatic on-push repair job in this repo.

Useful optional inputs:

```bash
gh workflow run "Mercury CI Auto-Repair Draft PR" \
  -f failure_command='cargo test --all-features --verbose' \
  -f base_ref='main' \
  -f setup_command='cargo fetch --locked' \
  -f lint_command='cargo clippy --all-targets --all-features -- -D warnings' \
  -f max_agents=20 \
  -f max_cost=0.5 \
  -f dry_run=true
```

Workflow behavior:

- creates an isolated detached worktree on the runner
- reproduces the baseline failure first
- runs `target/release/mercury-cli fix ... --noninteractive` only when baseline is red and API key is present
- repair targeting remains Rust-only (`cargo test`, `cargo check`, `cargo clippy` direct forms); unsupported command shapes may still produce artifacts but will not produce a verified repair
- TypeScript support in v1.0 lane is still partial until full engine integration lands; do not treat this workflow as TypeScript-parity repair yet
- uploads an evidence bundle and run summary for every terminal state
- only attempts branch push and draft-PR mutation when repair is verified, the diff is non-empty, and `dry_run != true`
- with `dry_run=true`, uploads evidence and run summary but skips branch push and PR mutation entirely

Manual fallback for the same isolated repair shape:

```bash
./target/release/mercury-cli fix "<exact ci failure summary>"
```

Then verify with the same command class used by CI, for example:

```bash
cargo test --all-features --verbose
```

## Step 3: Package Evidence

Collect the run artifacts so reviewers can audit the decision path.

For the workflow path, download the uploaded artifact bundle from the Actions run. The current workflow validates and uploads:

- `summary.md`
- `decision.json`
- `environment.json`
- `pr-body.md`
- `repair.diff`
- `repair.diffstat.txt`
- `git-status.txt`
- `logs/baseline.stdout.log` and `logs/baseline.stderr.log`
- `logs/repair.stdout.log`, `logs/repair.stderr.log`, `logs/post-repair.stdout.log`, and `logs/post-repair.stderr.log` when the repair path ran
- `logs/setup.stdout.log` and `logs/setup.stderr.log` when `setup_command` was used
- `logs/mercury-init.stdout.log` and `logs/mercury-init.stderr.log` when the workflow ran init
- copied `mercury-run/` when the workflow captured a nested `fix` run

When `mercury-run/` exists, it includes the normal `fix` evidence set (for example `plan.json`, `execution-summary.json`, `metadata.json`, `diff.patch` when present, and `audit.log`).

Minimum contract that must exist in every run artifact:

- `summary.md`
- `decision.json`
- `environment.json`
- `pr-body.md`
- `repair.diff`
- `repair.diffstat.txt`
- `logs/baseline.stdout.log`
- `logs/baseline.stderr.log`

For the manual path, archive:
- `.mercury/runs/<run-id>/plan.json`
- `.mercury/runs/<run-id>/assessments.json`
- `.mercury/runs/<run-id>/execution-summary.json`
- `.mercury/runs/<run-id>/final-verification.json` when present
- `.mercury/runs/<run-id>/agent-logs.json`
- `.mercury/runs/<run-id>/thermal-aggregates.json`
- `.mercury/runs/<run-id>/metadata.json`
- `.mercury/runs/<run-id>/diff.patch` when present
- `.mercury/runs/<run-id>/swarm-state.json` when present
- `.mercury/runs/<run-id>/audit.log`

If eval corpus checks are part of the branch gate, include:

```bash
python3 evals/v0/run.py --output-dir evals/v0/reports/ci
```

## Step 4: Promote a Verified Repair Into Draft PR

Inspect the uploaded artifact bundle first. The workflow only attempts the PR step after a verified repair, so treat the artifact bundle as the primary source of truth.

If `decision.json` shows `repair_verified=true` and you ran with `dry_run=false`, the workflow will attempt to push `pr_branch` and then open or update a draft PR using the generated `pr-body.md`. The PR body includes the run artifact URL.

If you used `dry_run=true`, no branch push or PR mutation occurs; use the uploaded artifact bundle for review-only triage.

If the workflow ran in a context that cannot push back to the repository or mutate PRs, keep the uploaded artifact bundle and fall back to the manual PR path below.

If you are running the same flow outside GitHub Actions, commit only the accepted repair edits, then open a draft PR manually and link the artifact bundle in the PR body.

```bash
git add <accepted-paths>
git commit -m "ci: mercury repair for <failure-id>"
git push origin <repair-branch>
gh pr create --draft --title "Mercury repair: <failure-id>" --body-file .github/PULL_REQUEST_TEMPLATE.md
```

Draft PR should include:
- failure reproduction details
- accepted diff summary
- verifier re-run output summary
- links to uploaded artifact bundle

## Success Criteria

Treat the case study as successful only if all are true:
- failing CI state is reproducible
- repaired state passes the target verifier locally and in CI
- evidence bundle is attached and reviewable
- when the repo context allowed mutation, the PR is draft and clearly marked machine-assisted; otherwise `dry_run=true` or a manual PR handoff was used explicitly

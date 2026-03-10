# Case Study: Local Red -> Green With `watch --repair`

This walkthrough shows the local flow supported in the current repo: watch a failing Rust verifier command, let Mercury attempt a bounded repair, and inspect the artifact bundle before deciding what to keep.

## Goal

Start from a known failing Rust target, run the real `watch --repair` loop, verify locally, and keep the run auditable.

## Prerequisites

- macOS or Linux with a Rust toolchain installed
- `INCEPTION_API_KEY` available
- Mercury CLI built from this repo (`./target/release/mercury-cli`) or installed via `cargo install --path .` (`mercury-cli`)
- a repo state where a direct Rust verifier command is currently failing

```bash
cargo build --release
export INCEPTION_API_KEY="<your-key>"
export MERCURY_API_KEY="$INCEPTION_API_KEY"
```

## Step 1: Reproduce the Failure

Create a temporary branch and confirm the exact Rust verifier command that is red.

```bash
git checkout -b demo/local-red-green
cargo test -p your-crate
```

Use the exact direct verifier command that fails. Current `watch --repair` targeting is limited to direct `cargo test`, `cargo check`, and `cargo clippy` invocations (including env-prefix variants that still resolve directly to those commands).

Examples that are supported:

```bash
cargo test -p your-crate
cargo check --workspace
cargo clippy -p your-crate --all-targets -- -D warnings
RUST_BACKTRACE=1 cargo test --quiet
```

Examples that are intentionally not auto-repaired:

```bash
cargo test && cargo clippy
cargo test | tee failing.log
just test
make test
```

## Step 2: Start the Watch-Repair Loop

Run `watch --repair` with the same direct Rust verifier command you just reproduced.

```bash
./target/release/mercury-cli init
./target/release/mercury-cli watch "cargo test -p your-crate" --repair
```

If you installed the binary, use:

```bash
mercury-cli init
mercury-cli watch "cargo test -p your-crate" --repair
```

For CI-safe log output, use `--noninteractive` (the GitHub workflow does this for `fix`).

What to expect:

- Mercury runs the watched command immediately.
- If the command is already green, the cycle is recorded as `passed_without_repair`.
- If the command is red and supported, Mercury invokes the existing `fix` flow with a verifier config targeted to that exact Rust command.
- If Mercury applies a fix, it reruns the watched command and records whether the result is now green.
- The loop then waits for the next repository change and repeats until you stop it.

## Step 3: Read the Decision and Artifact Path

At the end of each cycle, Mercury prints the artifact bundle location and the cycle decision.

Cycle decisions you may see:

- `passed_without_repair`
- `failed_without_repair`
- `repaired_and_verified`
- `repair_applied_but_command_still_failing`
- `repair_not_applied`
- `repair_flow_failed`
- `repair_not_supported`

A green local case study is the `repaired_and_verified` path.

## Step 4: Inspect the Bundle

Open the printed bundle under `.mercury/runs/<run-id>/`.

Expected watch-level artifacts:

- `watch.json`
- `initial.stdout.txt`
- `initial.stderr.txt`
- `initial.failure.json` when a structured failure parse is available for the first run
- `confirmation.stdout.txt` and `confirmation.stderr.txt` when Mercury reruns the verifier after repair
- `confirmation.failure.json` when a structured failure parse is available for the confirmation rerun
- `audit.log`

Expected nested repair artifacts when the fix flow ran:

- `repair/diff.patch`
- `repair/execution-summary.json`
- `repair/final-verification.json`
- `repair/metadata.json`
- `repair/plan.json` and `repair/grounded-context.json` when they exist in the source `fix` artifact root

If the fix run produced its own artifact root, `watch.json` also records that source bundle path. Open that source bundle when you need the full `fix` evidence set:

- `plan.json`
- `assessments.json`
- `execution-summary.json`
- `final-verification.json` when final verification ran
- `agent-logs.json`
- `thermal-aggregates.json`
- `metadata.json`
- `diff.patch` when an accepted patch was produced
- `swarm-state.json` when runtime state was captured
- `audit.log`

Minimum sanity check for a reproducible local handoff:

```bash
test -f .mercury/runs/<run-id>/watch.json
jq '.decision' .mercury/runs/<run-id>/watch.json
```

## Step 5: Verify the Repo State

Confirm that the verifier command is now green and that the worktree only contains accepted changes.

```bash
cargo test -p your-crate
git status --short
git diff
```

Expected outcomes:

- green: the verifier passes and the accepted edits remain in the worktree
- still red: inspect `watch.json` and the nested repair artifacts before deciding whether to keep iterating
- non-allowlisted watch command: the CLI rejects the command before running a watch cycle; rerun with a direct allowlisted verifier command

## Step 6: Promote the Result

If the local run is good enough to share, either commit the accepted change and use the separate CI case study for the draft PR handoff, or trigger the `Mercury CI Auto-Repair Draft PR` workflow when you want GitHub-hosted artifact publication and a conditional same-repo draft-PR attempt.

- Local watch-repair is implemented in the repo today.
- Manual artifact-backed draft PR flow is documented in [CI-oriented repair to draft PR flow](ci-draft-pr-flow.md).
- The `Mercury CI Auto-Repair Draft PR` workflow always uploads evidence, and only attempts branch push plus draft-PR mutation after a verified repair when `dry_run=false`.

## Current Limits

- `watch --repair` is Rust-first and only auto-repairs direct verifier commands.
- Shell pipelines, chained commands, and wrapper tools are deliberately outside the current auto-repair target surface.
- `--max-agents` should not be described as proof of real concurrent swarm execution yet.
- TypeScript runtime support is still partial and should not be treated as a parity path for this case study yet.

## Cleanup

```bash
git status --short
# Keep the accepted fix for follow-up commit, or discard the demo branch:
git checkout main
git branch -D demo/local-red-green
```

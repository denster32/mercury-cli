# CI Draft-PR Repair Starter

This starter repo is a copyable GitHub Actions demo for the current Mercury Rust beta lane. Read `docs/operator-quickstart.md` first, then use this repo when you want the canonical disposable CI onboarding path.

What it gives you:

- a tiny Rust crate with a reproducible failing `cargo test`
- a normal CI workflow that keeps the branch red until the bug is fixed
- a manual `workflow_dispatch` wrapper that calls the reusable Mercury draft-PR repair workflow pinned to `v1.0.0-beta.1`

## Before You Run It

1. Copy this directory into a new repository.
2. Add a repository secret named `INCEPTION_API_KEY` or `MERCURY_API_KEY`.
3. Push the repo so the baseline `CI` workflow shows the failing `cargo test`.

## Run The Repair Workflow

From GitHub Actions, trigger `Mercury Repair Draft PR` with the default inputs, or run:

```bash
gh workflow run "Mercury Repair Draft PR" \
  -f failure_command='cargo test --all-features --verbose' \
  -f repair_goal='Repair the failing Rust verifier and keep the change reviewable' \
  -f dry_run=true
```

What happens:

- the reusable Mercury workflow reproduces the failing verifier in isolation
- it runs the Rust-first `fix --noninteractive` repair path
- it uploads the artifact bundle even when repair is not verified
- when `dry_run=false` and the repair is verified, it can open or update a same-repo draft PR

## What To Inspect

Start with the uploaded artifact bundle and open:

- `artifact-index.json`
- `summary.md`
- `decision.json`
- `repair.diff`
- `repair.diffstat.txt`
- `mercury-run/` when present

## Current Limits

- this starter is intentionally pinned to `denster32/mercury-cli/.github/workflows/repair.yml@v1.0.0-beta.1`
- update that ref deliberately when you adopt a newer Mercury beta
- the supported repair lane is still direct Rust verifier commands, not generic shell workflows

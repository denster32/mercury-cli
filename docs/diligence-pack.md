# Diligence Pack

This page collects the repo-backed materials to hand to a skeptical design partner, evaluator, or buyer for the current `1.0.0-beta.1` branch line.

## Core Evidence

- [Operator quickstart](operator-quickstart.md): shortest operator path from install to first repair attempt
- [Starter repos](../starter-repos/README.md): canonical disposable onboarding repos for local watch-repair and CI draft-PR rehearsal
- [Quality contract](QUALITY.md): enforced CI, repair-workflow, and release-gate automation
- [Known limitations](known-limitations.md): bounded caveats for the current beta line
- [Supported Rust verifier classes](supported-rust-verifier-classes.md): current Rust beta support boundary

## Benchmark Truth

- [Benchmarks overview](benchmarks/README.md)
- [Public Rust Tier 1 benchmark report](benchmarks/rust-v0-repair-benchmark.md)
- [Quality report JSON](benchmarks/rust-v0-quality.report.json)
- [Agent-sweep report JSON](benchmarks/rust-v0-agent-sweep.report.json)

What those reports currently say:

- the public Rust beta truth is limited to `evals/v0/tier1-manifest.json`
- the checked-in `20260313-quality` and `20260313-agent-sweep` runs still report `0.0` verified repair rate, `0.0` accepted patch rate, and `0.0` false-green rate
- Tier 0 and Tier 2 remain internal diagnostic slices for failure explanation, not broader support claims

## Workflow Evidence

- [Local red -> green watch-repair flow](case-studies/local-red-to-green.md)
- [CI-oriented repair to draft PR flow](case-studies/ci-draft-pr-flow.md)
- `.github/workflows/repair.yml`: CI repair orchestration and evidence contract
- `.github/workflows/release.yml`: prerelease packaging with Tier 1 benchmark gate before packaging

For a live diligence packet, attach:

- one CI artifact bundle containing `artifact-index.json`, `summary.md`, `decision.json`, `environment.json`, `repair.diff`, `repair.diffstat.txt`, and any nested `mercury-run/` evidence
- one local `.mercury/runs/<run-id>/summary-index.json` bundle from `watch --repair`
- one screenshot of the GitHub Actions run summary plus the uploaded artifact list from that same run

## Release and Policy Docs

- [CHANGELOG.md](../CHANGELOG.md)
- [SECURITY.md](../SECURITY.md)
- [CONTRIBUTING.md](../CONTRIBUTING.md)
- [LICENSE](../LICENSE)
- [COMMERCIAL_LICENSE.md](../COMMERCIAL_LICENSE.md)

Use this pack to show workflow quality, benchmark methodology, artifact traces, and release discipline. Do not use it to imply repair efficacy beyond the checked-in Tier 1 reports.

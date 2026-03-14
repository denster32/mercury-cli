# Supported Rust Verifier Classes

This page defines the current Rust beta verifier contract for `1.0.0-beta.1`.

The public support claim is tied to:

- `evals/v0/tier1-manifest.json` as the public Rust beta lane
- `docs/benchmarks/rust-v0-quality.report.json` and `docs/benchmarks/rust-v0-agent-sweep.report.json` as the checked-in benchmark truth for that lane

## Tier 1 Beta Contract

The supported Rust verifier classes for the current beta story are:

- `cargo test`
- `cargo check`
- `cargo clippy`

Supported command shapes:

- direct invocations of the commands above
- env-prefix variants that still resolve directly to those commands, for example `RUST_BACKTRACE=1 cargo test --quiet`

Not part of the supported command contract:

- shell pipelines, `&&`, `||`, subshells, or other shell composition
- wrapper scripts or helper commands that hide the verifier class
- non-Rust verifier commands in the local `watch --repair` path

## Where Each Class Is Supported

| Surface | `cargo test` | `cargo check` | `cargo clippy` |
| --- | --- | --- | --- |
| Local `watch --repair` | Yes | Yes | Yes |
| Direct `fix` repair flow | Yes | Yes | Yes |
| `.github/workflows/repair.yml` | Yes | Yes | Yes |
| TypeScript experimental lane | No | No | No |

## Benchmark Tie-In

The Tier 1 reports are the efficacy truth for these supported classes. As of the checked-in `20260313-quality` and `20260313-agent-sweep` reports:

- `verified_repair_rate`: `0.0`
- `accepted_patch_rate`: `0.0`
- `false_green_rate`: `0.0`

Those numbers mean the verifier classes above are the current support boundary, not proof of effective repair outcomes yet.

## How To Use This Contract

- Start with [Operator quickstart](operator-quickstart.md) if you are choosing a workflow.
- Use [Known limitations](known-limitations.md) for the bounded caveats that still apply inside this verifier surface.
- Use [docs/benchmarks/README.md](benchmarks/README.md) when you need the public report methodology and checked-in benchmark assets.

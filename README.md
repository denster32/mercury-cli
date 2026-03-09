# Mercury CLI

Mercury CLI is Mercury-native CI auto-repair for teams using Inception Labs models.

The product promise is narrow on purpose: take a failing verification loop, generate bounded Mercury repair candidates, verify them in disposable workspaces, and leave behind enough evidence to audit the decision later. v0.2 is the trustworthy local repair core for that workflow. It is not yet a generic agent shell, a draft-PR bot, or a proven concurrent swarm runtime.

## Install

### From source

```bash
git clone https://github.com/denster32/mercury-cli
cd mercury-cli
cargo build --release
```

### From GitHub Releases

The repo includes tagged release workflows for macOS and Linux archives. Until the first tagged release is published, the source build path above is the supported install path.

### API key

Mercury CLI documentation uses `INCEPTION_API_KEY` as the preferred environment variable. `MERCURY_API_KEY` still works as a backward-compatible fallback.

```bash
export INCEPTION_API_KEY="your-api-key"
# Optional fallback for older configs or older CLI help text:
export MERCURY_API_KEY="$INCEPTION_API_KEY"
```

Compatibility note: some current config templates and help text in the CLI still reference `MERCURY_API_KEY`. The runtime should accept both names while the remaining surfaces are being aligned.

## 60-Second Demo

The current fastest path is local: reproduce the failure, run Mercury repair, inspect the evidence bundle, and keep only verified changes.

```bash
git clone https://github.com/denster32/mercury-cli
cd mercury-cli
cargo build --release

export INCEPTION_API_KEY="your-api-key"
export MERCURY_API_KEY="$INCEPTION_API_KEY"

./target/release/mercury init
./target/release/mercury plan "fix the failing auth retry tests"
./target/release/mercury fix "cargo test fails in auth retry logic"
./target/release/mercury status --heatmap
```

What a good v0.2 run should leave behind:

- a verified diff or accepted patch
- local verifier output for the parse, test, or lint gates Mercury used
- run metadata and artifacts under `.mercury/runs/`
- no partial writes left behind by rejected candidates

## Capability Matrix

| Surface | Status | Reality in the current repo |
| --- | --- | --- |
| `mercury init` | Available | Creates `.mercury/` config and thermal database. |
| `mercury plan <goal>` | Available | Produces a structured repair plan and thermal assessments. |
| `mercury ask <query>` | Available | Repo-aware Mercury 2 Q&A. |
| `mercury status [--heatmap] [--agents] [--budget]` | Available | Reports thermal state and scheduler metadata. |
| `mercury edit apply` | Available | Apply-style Mercury Edit request for focused mutation. |
| `mercury edit complete` | Available | Completion-style Mercury Edit request for a file or cursor location. |
| `mercury edit next` | Available | Next-edit prediction using current file state and edit history. |
| `mercury fix <description>` | Available | Rust-first repair flow with planning, candidate generation, sandbox verification, and artifacts. |
| `mercury fix --max-agents N` | Available with limits | Changes scheduling and budget parameters today. Do not market it as proof of real swarm speedup until the concurrent executor is measured and shipped. |
| `mercury watch <command> --repair` | Preview | Command watching exists; the end-to-end autonomous repair loop is still being completed. |
| `mercury config get` / `validate` | Available | Reads or validates config values. |
| `mercury config set` | Preview | CLI surface exists, but direct TOML editing may still be the more dependable path. |
| GitHub Action draft PR bot | Roadmap | Planned for CI auto-repair alpha, not part of v0.2. |
| Generic workflow DSL / `agent run` | Not planned before v1.0 | Intentionally deferred until the repair core is proven. |

## Safety Model

Mercury CLI should be evaluated like a repair system, not a chatbot shell.

### Candidate isolation

Repair candidates run in disposable workspaces under `.mercury/worktrees/` instead of mutating the user worktree directly.

### Atomic acceptance

Rejected candidates are discarded with the workspace. Accepted candidates are copied back only after local verification succeeds.

### Local verification first

Parse, test, and lint commands are local gates. Model output does not become an accepted repository change until those gates pass.

### Structured output boundary

Planner and critique outputs are treated as data rather than prose. The API layer now supports official-style strict JSON schema requests, and the remaining runtime callsites are being hardened around that contract as part of v0.2.

### Reproducible evidence

Each repair run should leave behind an artifact bundle under `.mercury/runs/` with the plan, candidate diffs, verifier output, timing, and cost metadata.

## What v0.2 Supports

- Rust repositories first
- local repair loops that can be promoted into CI later
- Mercury 2 for planning and critique
- Mercury Edit for focused edits
- isolated verification and accepted-change copy-back
- explicit preview labels for incomplete surfaces

## Preview and Roadmap

### Preview in v0.2

- `watch --repair` as a hands-off repair loop
- `config set` as a fully dependable config-editing surface
- binary install via tagged release until the first public release exists

### Roadmap after v0.2

- GitHub Action that reproduces failures and opens draft PRs with evidence bundles
- true multi-worktree concurrent candidate execution
- conflict arbitration across overlapping edits
- a second supported language after the Rust-first path
- live observability for active candidates, cost, and acceptance decisions

## What v0.2 Does Not Claim

- real swarm speedup from `--max-agents`
- safe parallel candidate execution across many worktrees
- GitHub draft PR creation
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

Compatibility note: older configs may still reference `MERCURY_API_KEY`. v0.2 docs prefer `INCEPTION_API_KEY` and treat the older name as a fallback.

## Artifact Bundle

A trustworthy repair run leaves behind enough evidence to replay the decision:

- planner request and response metadata
- candidate diffs for generated edits
- verifier commands and outputs
- final accept or reject decision
- timing and cost summary

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

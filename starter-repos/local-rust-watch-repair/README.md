# Local Rust Watch-Repair Starter

This starter repo is the smallest local Mercury beta rehearsal: a single Rust crate with one intentionally failing unit test.

Why it starts red:

- the implementation keeps the original letter casing
- the test expects a lowercase kebab-case label

That gives you a direct `cargo test` failure, which is the narrow local auto-repair lane Mercury currently supports.

## Run It

```bash
cd starter-repos/local-rust-watch-repair
cargo test

mercury-cli init
mercury-cli watch "cargo test" --repair
```

If you built the binary from this repo instead of installing it:

```bash
../../target/release/mercury-cli init
../../target/release/mercury-cli watch "cargo test" --repair
```

## What To Inspect

After a cycle completes, open the latest run bundle under `.mercury/runs/` and start with:

- `summary-index.json`
- `watch.json`
- `initial.stdout.txt`
- `confirmation.stdout.txt` when repair ran
- `repair/execution-summary.json`

## Current Limits

- this starter is for local Rust `watch --repair`, not the CI draft-PR flow
- use a direct Rust verifier command only; do not wrap it in shell pipelines or helper tools
- the crate is intentionally trivial so you can validate workflow and artifact handling before trying a real repo

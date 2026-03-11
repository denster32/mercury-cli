# Contributing

Mercury CLI is trying to become a trustworthy repair tool, not just a model wrapper. Contributions should improve correctness, safety, evidence quality, or release quality before they expand the surface area.

## Development setup

```bash
git clone https://github.com/denster32/mercury-cli
cd mercury-cli
cargo build
```

Set an API key for local testing:

```bash
export INCEPTION_API_KEY="your-api-key"
export MERCURY_API_KEY="$INCEPTION_API_KEY"
```

## Before opening a pull request

Run the local checks:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --verbose
```

If your change affects repair execution, also include targeted evidence for the changed behavior. Good evidence includes:

- same-file multi-step edit regression coverage
- proof that failed verification does not dirty the user worktree
- artifact bundle examples for accept and reject paths
- request or response fixtures for structured Mercury API payloads
- release or packaging output when changing distribution workflows

## Contribution priorities

Before proposing new surface area, check whether the work strengthens one of these priorities:

- sandboxed candidate verification
- atomic acceptance and rollback safety
- strict schema validation for model output
- protocol-correct Mercury Edit requests
- reproducible evals and artifact bundles
- honest product docs and shippable release workflows

## Documentation and claims

Keep docs and release notes honest.

- mark incomplete features as `Preview` or `Roadmap`
- avoid claiming true parallel swarm behavior until benchmarks prove it
- prefer concrete runtime behavior over theory-heavy language

## Issue reporting

Use the issue templates when possible:

- bug report: broken behavior or regression
- eval case: reproducible repair benchmark or failure corpus addition
- feature request: scoped product or workflow proposal

Security issues should follow [SECURITY.md](SECURITY.md) and should not be filed publicly.

## Release process

Tagged releases use the `vX.Y.Z` format.

Release checklist:

1. update `CHANGELOG.md`
2. confirm CI is green
3. confirm docs match actual behavior
4. create and push a tag such as `v1.0.0`, or run the release workflow manually with an explicit version
5. verify the release workflow uploaded macOS and Linux archives plus checksum files
6. smoke-test the packaged `mercury` command from one release archive before announcing the tag

## Working style

Be direct, reproducible, and respectful. When discussing a change, prioritize evidence over speculation.

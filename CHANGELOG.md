# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows Semantic Versioning for tagged releases.

## [Unreleased]

### Added

- release-ready GitHub Actions workflows for CI, build artifacts, and tagged releases
- issue templates for bugs, eval corpus additions, and scoped feature requests
- contributor and security policy documents for triage, release, and disclosure flow
- Mercury-native CI auto-repair runtime with isolated candidate execution, atomic acceptance, and run artifact bundles
- TypeScript eval-lane coverage alongside the Rust repair and verifier path
- checked-in Rust benchmark publication assets under `docs/benchmarks/` plus the `evals/repair_benchmark/publish.py` generator

### Changed

- release archives now package the built `mercury-cli` binary as the `mercury` command plus top-level project docs
- manual release dispatch now validates that the requested version matches `Cargo.toml`
- project hygiene now defaults to issue templates and private security reporting instead of blank issues
- CI auto-repair product docs and workflow copy now describe draft-PR creation as a conditional same-repository promotion step, with evidence bundles remaining the primary output for every run
- branch-head release truth now stays on `1.0.0-beta.1`, `config set` updates documented scalar keys safely, `edit apply` requires concrete snippet content, and benchmark reporting publishes scoped runtime/evidence caveats instead of pending-proof language

## [1.0.0-beta.1] - 2026-03-13

### Added

- `scripts/install.sh` for tagged release installs with explicit prerelease version selection and platform detection for the current macOS arm64 and Linux x86_64 release matrix
- downloadable `mercury-benchmarks-<version>.tar.gz` release assets containing the checked-in public Rust benchmark publication set plus the Tier 0, Tier 1, and Tier 2 Rust manifests that accompany that report surface

### Changed

- tagged release archives now ship both `mercury-cli` and a `mercury` compatibility alias to reduce command-name friction between source builds and release installs
- README install guidance now separates prerelease and future stable install paths, points operators first to the quickstart plus starter repos, and makes the current Windows source-build-only status explicit
- CI repair artifact bundles now publish a stable top-level `artifact-index.json`, and the public docs link the bounded verifier-class, limitations, and diligence pages instead of relying on broad narrative copy
- the checked-in Tier 1 Rust benchmark truth for this prerelease remains `0.0` verified repair rate, `0.0` accepted patch rate, and `0.0` false-green rate in run ids `20260313-quality` and `20260313-agent-sweep`; release gating now treats those checked-in benchmark deltas as the prerelease truth and blocks future regressions once an earlier tagged Tier 1 baseline exists

### Upgrade Notes

- treat `v1.0.0-beta.1` as the exact Rust beta runtime surface, not a general stable release promise
- use `scripts/install.sh --version v1.0.0-beta.1` when you want the current beta instead of whichever stable release tag is latest
- official tagged binaries remain limited to macOS arm64 and Linux x86_64; Windows users should continue with source builds until an official Windows release archive exists
- release assets now include a separate benchmark publication bundle so reviewers can download the public Tier 1 Rust report set without cloning the repo

## [Initial public crate scaffolding]

### Added

- initial Mercury CLI command surface for `init`, `plan`, `status`, `ask`, `edit`, `fix`, `watch`, and config inspection
- Rust crate packaging and CI workflow with format, lint, check, and test jobs

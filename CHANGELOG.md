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

## [0.1.0]

### Added

- initial Mercury CLI command surface for `init`, `plan`, `status`, `ask`, `edit`, `fix`, `watch`, and config inspection
- Rust crate packaging and CI workflow with format, lint, check, and test jobs

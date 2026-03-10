# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project aims to follow Semantic Versioning once release tagging begins.

## [Unreleased]

### Added

- release-ready GitHub Actions workflows for CI, build artifacts, and tagged releases
- issue templates for bugs, eval corpus additions, and scoped feature requests
- contributor and security policy documents for triage, release, and disclosure flow

### Changed

- release archives now package the built `mercury-cli` binary as the `mercury` command plus top-level project docs
- manual release dispatch now supports an explicit version input instead of assuming a tag context
- project hygiene now defaults to issue templates and private security reporting instead of blank issues
- CI auto-repair product docs and workflow copy now describe draft-PR creation as a conditional same-repository promotion step, with evidence bundles remaining the primary output for every run

## [0.1.0]

### Added

- initial Mercury CLI command surface for `init`, `plan`, `status`, `ask`, `edit`, `fix`, `watch`, and config inspection
- Rust crate packaging and CI workflow with format, lint, check, and test jobs

# Security Policy

## Supported versions

Mercury CLI is pre-1.0. Security fixes are handled on a best-effort basis for:

- the latest commit on `main`
- the most recent tagged release, once releases begin

Older snapshots may not receive backports.

## Reporting a vulnerability

Do not open public GitHub issues for security-sensitive bugs.

Preferred path:

1. use GitHub's private vulnerability reporting for this repository
2. include the affected version or commit, reproduction steps, impact, and any required environment details
3. if the issue involves logs or artifacts, redact secrets before attaching them

If private reporting is unavailable, contact the maintainers privately through GitHub instead of opening a public issue.

## What to include

A good report includes:

- affected command or workflow
- exact local command or CI trigger
- expected behavior
- actual behavior
- whether secrets, filesystem integrity, or arbitrary command execution are involved
- whether the issue can dirty a repository or bypass verification gates
- whether the issue affects release archives, installers, or shipped checksums

## Response targets

Best-effort targets:

- acknowledgement within 7 days
- mitigation guidance or triage status within 14 days for confirmed reports

## High-priority scope

High-priority issues include:

- sandbox escape or unsafe write behavior
- secret leakage in logs, artifacts, prompts, or release bundles
- command execution beyond documented verification surfaces
- acceptance of unverified or schema-invalid model output
- compromised release artifacts or incorrect checksum publication

# Mercury Starter Repos

Start with `docs/operator-quickstart.md` after install, then copy one of these starter repos when you want the canonical disposable onboarding path for the current Rust beta surface.

- `local-rust-watch-repair/`: local `watch --repair` starter around a direct `cargo test` failure
- `ci-draft-pr-repair/`: GitHub Actions starter that pins the reusable Mercury draft-PR repair workflow at `v1.0.0-beta.1`

They are not benchmark evidence and they are not a broader language-support claim. Treat them as the fastest way to rehearse the two operator-ready flows documented in:

- `docs/operator-quickstart.md`
- `docs/case-studies/local-red-to-green.md`
- `docs/case-studies/ci-draft-pr-flow.md`

Current truth:

- both starters are Rust-only on purpose
- both starters begin in a reproducible red state so you can exercise the repair path immediately
- the CI starter inherits the same direct verifier-command limits as the main repo workflow

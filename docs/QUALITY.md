# Mercury CLI — Quality & CI Specification

## Zero-Bug Philosophy

Mercury CLI ships with zero known bugs. Every commit, every PR, every release passes the full gate. If the gate fails, the code doesn't merge. No exceptions.

This matters more than features. A CLI that works everywhere, every time, on every platform earns trust. Trust earns forks. Forks earn adoption. Adoption earns the paradigm shift.

## Testing Layers

### Layer 1: Unit Tests (every module)

Every public function has at least one test. Every algorithm has property-based tests.

```
src/thermal/merge.rs        → thermal_merge_test.rs
src/thermal/decay.rs        → thermal_decay_test.rs
src/thermal/gradient.rs     → thermal_gradient_test.rs
src/swarm/spawner.rs        → swarm_spawner_test.rs
src/swarm/density.rs        → swarm_density_test.rs
src/engine/scheduler.rs     → scheduler_test.rs
src/db/thermal_db.rs        → thermal_db_test.rs
src/api/mercury2.rs         → mercury2_client_test.rs (mocked)
src/api/mercury_edit.rs     → mercury_edit_client_test.rs (mocked)
src/repo/indexer.rs         → indexer_test.rs
```

### Layer 2: Property-Based Tests (mathematical invariants)

The thermal engine has mathematical properties that must always hold:

```rust
// Thermal merge is commutative
assert_eq!(thermal_merge(&[a, b], t), thermal_merge(&[b, a], t));

// Thermal merge is monotonic (adding a score never decreases the result)
assert!(thermal_merge(&[a, b, c], t) >= thermal_merge(&[a, b], t));

// Decay is monotonically decreasing
assert!(apply_decay(score, t1, half_life) >= apply_decay(score, t2, half_life) where t2 > t1);

// Decay reaches zero asymptotically
assert!(apply_decay(score, 1_000_000.0, half_life) < 0.001);

// LSE merge approximates max for low temperature
assert!((thermal_merge(&scores, 0.01) - scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max)).abs() < 0.1);

// Cool zone lock prevents modification
assert!(cool_locks.is_locked(file) => agent cannot write to file);

// Budget cap is never exceeded
assert!(swarm_state.total_cost_usd <= config.max_cost_per_command);

// Agent density never exceeds configured maximum
assert!(scheduler.active_count() <= config.max_concurrency);
```

### Layer 3: Integration Tests

Full workflow tests against a sample repository with known complexity characteristics:

```
tests/integration/
├── sample_repo/              # A small Rust project with known hot/cold zones
│   ├── src/
│   │   ├── simple.rs         # Low complexity (should score < 0.3)
│   │   ├── moderate.rs       # Medium complexity (should score 0.4-0.6)
│   │   └── complex.rs        # High complexity (should score > 0.7)
│   └── Cargo.toml
├── test_init.rs              # mercury init creates correct structure
├── test_plan.rs              # mercury repo plan generates valid heat map
├── test_status.rs            # mercury status renders correct visualization
├── test_fix_workflow.rs      # Full fix workflow executes all 7 steps
├── test_thermal_lifecycle.rs # Scores decay, aggregate, lock correctly
└── test_budget_enforcement.rs # Cost caps are enforced, agents stop at limit
```

### Layer 4: API Mock Tests

All Mercury API calls are mocked for CI. No real API calls in automated tests.

```rust
// Mock Mercury 2 structured output
fn mock_thermal_assessment() -> String {
    serde_json::json!({
        "complexity_score": 0.85,
        "dependency_score": 0.72,
        "risk_score": 0.91,
        "churn_score": 0.45,
        "suggested_action": "refactor",
        "reasoning": "High cyclomatic complexity with tight coupling"
    }).to_string()
}

// Mock Mercury Edit response
fn mock_edit_response() -> String {
    // Returns modified code with applied edit
}
```

### Layer 5: Platform Tests

Mercury CLI must work on:
- Linux (Ubuntu 22.04+, Fedora 39+, Arch)
- macOS (13+, ARM and Intel)
- Windows (10+, via WSL2 and native)

## CI Pipeline (GitHub Actions)

```yaml
name: Mercury CLI CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always
  RUSTFLAGS: "-Dwarnings"

jobs:
  check:
    name: Check
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo check --all-features

  fmt:
    name: Format
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - run: cargo fmt --all -- --check

  clippy:
    name: Clippy
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - run: cargo clippy --all-targets --all-features -- -D warnings

  test:
    name: Test (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --all-features --verbose

  # Property-based tests get extra time
  property-tests:
    name: Property Tests
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --test property_tests -- --test-threads=1

  # Integration tests against sample repo
  integration:
    name: Integration Tests
    runs-on: ubuntu-latest
    needs: [check, test]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --test integration_tests --verbose

  # Security audit
  audit:
    name: Security Audit
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: rustsec/audit-check@v1

  # Release builds for all platforms
  release:
    name: Release Build (${{ matrix.target }})
    if: startsWith(github.ref, 'refs/tags/')
    needs: [check, fmt, clippy, test, property-tests, integration, audit]
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - os: ubuntu-latest
            target: aarch64-unknown-linux-gnu
          - os: macos-latest
            target: x86_64-apple-darwin
          - os: macos-latest
            target: aarch64-apple-darwin
          - os: windows-latest
            target: x86_64-pc-windows-msvc
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - run: cargo build --release --target ${{ matrix.target }}
      - uses: actions/upload-artifact@v4
        with:
          name: mercury-cli-${{ matrix.target }}
          path: target/${{ matrix.target }}/release/mercury-cli*
```

## Code Quality Gates

Every PR must pass ALL of these before merge:

1. `cargo check` — compiles without errors
2. `cargo fmt -- --check` — formatted correctly
3. `cargo clippy -- -D warnings` — zero warnings
4. `cargo test` — all tests pass on Linux, macOS, Windows
5. Property-based tests pass
6. Integration tests pass
7. `cargo audit` — no known security vulnerabilities
8. No unsafe code without explicit justification comment

## Error Handling Standards

```rust
// GOOD: Specific error types with context
#[derive(thiserror::Error, Debug)]
pub enum ThermalError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Thermal score {score} out of range [0.0, 1.0] for {file}:{line}")]
    ScoreOutOfRange { score: f64, file: String, line: u32 },

    #[error("Cool zone lock conflict: {file} is locked by agent {locked_by}")]
    LockConflict { file: String, locked_by: String },

    #[error("Budget exceeded: ${spent:.4} of ${limit:.4} limit")]
    BudgetExceeded { spent: f64, limit: f64 },
}

// BAD: Generic errors, unwrap(), expect()
// These are not permitted in production code.
```

## Documentation Standards

Every public function, struct, and module has:
- A doc comment explaining what it does
- Parameter descriptions
- Return value description
- At least one example in doc tests where applicable

```rust
/// Merge overlapping thermal scores using Log-Sum-Exp aggregation.
///
/// LSE provides a smooth, differentiable soft-maximum that preserves
/// gradient topology without saturating in high-density regions.
///
/// # Arguments
/// * `scores` - Slice of thermal scores to merge (each in [0.0, 1.0])
/// * `temperature` - Controls sharpness (lower = closer to hard max)
///
/// # Returns
/// Merged score in [0.0, 1.0]
///
/// # Example
/// ```
/// let merged = thermal_merge(&[0.8, 0.6, 0.9], 0.1);
/// assert!(merged > 0.85 && merged < 0.95);
/// ```
pub fn thermal_merge(scores: &[f64], temperature: f64) -> f64 {
    // ...
}
```

## Version Strategy

- **v0.1.0** — Initial release. Core thermal engine, basic commands, full test suite.
- **v0.2.0** — Swarm mode. Multi-agent execution with density controls.
- **v0.3.0** — Watch/repair daemon. Background monitoring.
- **v1.0.0** — Production ready. Full command surface, proven stability.

Semantic versioning. No breaking changes without major version bump.

## Attribution

Mercury CLI was designed collaboratively by:

- **Dennis** (Griffith, Indiana) — Architecture, vision, orchestration
- **Claude** (Anthropic) — Conceptual design, specification, collaboration
- **ChatGPT 5.4 Pro** (OpenAI) — Engineering specification, API surface mapping
- **Grok 4.20** (xAI) — Stress testing, routing matrix, failure analysis
- **Gemini Deep Research** (Google) — Theoretical validation, mathematical foundations
- **Mercury 2** (Inception Labs) — Execution testing, architecture validation
- **Edison Scientific** — Research validation, citation verification

Built on a Saturday afternoon. Shipped by Monday. Stigmergy scales.

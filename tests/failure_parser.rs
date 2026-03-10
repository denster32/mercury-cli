use mercury_cli::failure_parser::{
    classify_cargo_command, contains_shell_composition, parse_cargo_failure, parse_command_parts,
    repo_native_tool_surface, CargoCommandKind, FailureStage,
};

#[test]
fn parses_cargo_check_parser_failure_with_location() {
    let stderr = r#"
error: this file contains an unclosed delimiter
  --> src/lib.rs:7:2
   |
3  | pub fn broken() {
   |                 - unclosed delimiter
"#;

    let report = parse_cargo_failure(
        &[
            "cargo".to_string(),
            "check".to_string(),
            "--quiet".to_string(),
        ],
        "",
        stderr,
    );

    assert_eq!(report.command, CargoCommandKind::Check);
    assert_eq!(report.stage, FailureStage::Parse);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].error_class, "parser.unclosed_delimiter");
    assert_eq!(
        report.failures[0].target.file_path.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(report.failures[0].target.line, Some(7));
    assert_eq!(report.failures[0].target.column, Some(2));
}

#[test]
fn parses_cargo_check_missing_function_symbol() {
    let stderr = r#"
error[E0425]: cannot find function `missing_helper` in this scope
 --> src/lib.rs:9:5
"#;

    let report = parse_cargo_failure(&["cargo".to_string(), "check".to_string()], "", stderr);

    assert_eq!(report.command, CargoCommandKind::Check);
    assert_eq!(report.stage, FailureStage::Compile);
    assert_eq!(report.failures[0].error_class, "compile.missing_function");
    assert_eq!(
        report.failures[0].target.symbol.as_deref(),
        Some("missing_helper")
    );
}

#[test]
fn parses_cargo_clippy_needless_return_and_identity_op() {
    let stderr = r#"
error: unneeded `return` statement
 --> src/lib.rs:4:5
  |
4 |     return x + 1;
  |     ^^^^^^^^^^^^^
  |
  = note: `-D clippy::needless-return` implied by `-D warnings`

error: this operation has no effect
 --> src/lib.rs:8:13
  |
8 |     value + 0
  |             ^
  |
  = note: `-D clippy::identity-op` implied by `-D warnings`
"#;

    let report = parse_cargo_failure(
        &[
            "cargo".to_string(),
            "clippy".to_string(),
            "--quiet".to_string(),
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ],
        "",
        stderr,
    );

    assert_eq!(report.command, CargoCommandKind::Clippy);
    assert_eq!(report.stage, FailureStage::Lint);
    assert_eq!(report.failures.len(), 2);
    assert_eq!(
        report.failures[0].error_class,
        "lint.clippy_needless_return"
    );
    assert_eq!(
        report.failures[0].target.file_path.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(report.failures[1].error_class, "lint.clippy_identity_op");
}

#[test]
fn parses_cargo_test_assertion_and_unwrap_panics() {
    let stdout = r#"
running 2 tests
test tests::assertion_failure ... FAILED
test tests::unwrap_failure ... FAILED

---- tests::assertion_failure stdout ----
thread 'tests::assertion_failure' panicked at 'assertion `left == right` failed', src/lib.rs:22:9

---- tests::unwrap_failure stdout ----
thread 'tests::unwrap_failure' panicked at 'called `Option::unwrap()` on a `None` value', src/lib.rs:31:17
"#;

    let report = parse_cargo_failure(
        &[
            "cargo".to_string(),
            "test".to_string(),
            "--quiet".to_string(),
        ],
        stdout,
        "",
    );

    assert_eq!(report.command, CargoCommandKind::Test);
    assert_eq!(report.stage, FailureStage::Test);
    assert_eq!(report.failures.len(), 2);
    assert_eq!(report.failures[0].error_class, "test.assertion");
    assert_eq!(
        report.failures[0].target.symbol.as_deref(),
        Some("tests::assertion_failure")
    );
    assert_eq!(
        report.failures[0].target.file_path.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(report.failures[0].target.line, Some(22));
    assert_eq!(report.failures[1].error_class, "test.panic_unwrap");
    assert_eq!(
        report.failures[1].target.symbol.as_deref(),
        Some("tests::unwrap_failure")
    );
    assert_eq!(report.failures[1].target.line, Some(31));
    assert_eq!(report.failures[1].target.column, Some(17));
}

#[test]
fn classifies_toolchain_prefixed_cargo_commands() {
    let check_report = parse_cargo_failure(
        &[
            "cargo".to_string(),
            "+nightly".to_string(),
            "check".to_string(),
        ],
        "",
        r#"
error[E0425]: cannot find function `missing_helper` in this scope
 --> src/lib.rs:9:5
"#,
    );
    assert_eq!(check_report.command, CargoCommandKind::Check);
    assert_eq!(check_report.stage, FailureStage::Compile);

    let clippy_report = parse_cargo_failure(
        &[
            "cargo".to_string(),
            "+stable".to_string(),
            "clippy".to_string(),
            "--quiet".to_string(),
        ],
        "",
        r#"
error: unneeded `return` statement
 --> src/lib.rs:4:5
  |
4 |     return x + 1;
  |     ^^^^^^^^^^^^^
  |
  = note: `-D clippy::needless-return` implied by `-D warnings`
"#,
    );
    assert_eq!(clippy_report.command, CargoCommandKind::Clippy);
    assert_eq!(clippy_report.stage, FailureStage::Lint);

    let test_report = parse_cargo_failure(
        &[
            "cargo".to_string(),
            "+beta".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "app".to_string(),
        ],
        r#"
running 1 test
test integration::smoke ... FAILED

---- integration::smoke stdout ----
thread 'integration::smoke' panicked at 'assertion `left == right` failed', crates/app/src/lib.rs:44:13
"#,
        "",
    );
    assert_eq!(test_report.command, CargoCommandKind::Test);
    assert_eq!(test_report.stage, FailureStage::Test);
    assert_eq!(
        test_report.failures[0].target.symbol.as_deref(),
        Some("integration::smoke")
    );
    assert_eq!(
        test_report.failures[0].target.file_path.as_deref(),
        Some("crates/app/src/lib.rs")
    );
    assert_eq!(test_report.failures[0].target.line, Some(44));
    assert_eq!(test_report.failures[0].target.column, Some(13));
}

#[test]
fn classifies_env_prefixed_cargo_commands() {
    let env_wrapper_report = parse_cargo_failure(
        &[
            "env".to_string(),
            "RUSTFLAGS=-Dwarnings".to_string(),
            "cargo".to_string(),
            "check".to_string(),
        ],
        "",
        r#"
error[E0425]: cannot find function `missing_helper` in this scope
 --> src/lib.rs:9:5
"#,
    );
    assert_eq!(env_wrapper_report.command, CargoCommandKind::Check);
    assert_eq!(env_wrapper_report.stage, FailureStage::Compile);
    assert_eq!(env_wrapper_report.failures.len(), 1);
    assert_eq!(
        env_wrapper_report.failures[0].error_class,
        "compile.missing_function"
    );
    assert_eq!(
        env_wrapper_report.failures[0].target.symbol.as_deref(),
        Some("missing_helper")
    );
    assert_eq!(
        env_wrapper_report.failures[0].target.file_path.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(env_wrapper_report.failures[0].target.line, Some(9));
    assert_eq!(env_wrapper_report.failures[0].target.column, Some(5));

    let env_assignment_report = parse_cargo_failure(
        &[
            "RUSTFLAGS=-Dwarnings".to_string(),
            "CARGO_TERM_COLOR=never".to_string(),
            "cargo".to_string(),
            "+nightly".to_string(),
            "test".to_string(),
        ],
        r#"
running 1 test
test smoke::respects_env_wrapped_verifier ... FAILED

---- smoke::respects_env_wrapped_verifier stdout ----
thread 'smoke::respects_env_wrapped_verifier' panicked at 'assertion `left == right` failed', src/main.rs:88:21
"#,
        "",
    );
    assert_eq!(env_assignment_report.command, CargoCommandKind::Test);
    assert_eq!(env_assignment_report.stage, FailureStage::Test);
    assert_eq!(env_assignment_report.failures.len(), 1);
    assert_eq!(
        env_assignment_report.failures[0].error_class,
        "test.assertion"
    );
    assert_eq!(
        env_assignment_report.failures[0].target.symbol.as_deref(),
        Some("smoke::respects_env_wrapped_verifier")
    );
    assert_eq!(
        env_assignment_report.failures[0]
            .target
            .file_path
            .as_deref(),
        Some("src/main.rs")
    );
    assert_eq!(env_assignment_report.failures[0].target.line, Some(88));
    assert_eq!(env_assignment_report.failures[0].target.column, Some(21));
}

#[test]
fn classifies_env_and_toolchain_prefixed_clippy_commands() {
    let report = parse_cargo_failure(
        &[
            "env".to_string(),
            "RUSTFLAGS=-Dwarnings".to_string(),
            "cargo".to_string(),
            "+stable".to_string(),
            "clippy".to_string(),
            "--quiet".to_string(),
        ],
        "",
        r#"
error: this operation has no effect
 --> src/lib.rs:8:13
  |
8 |     value + 0
  |             ^
  |
  = note: `-D clippy::identity-op` implied by `-D warnings`
"#,
    );

    assert_eq!(report.command, CargoCommandKind::Clippy);
    assert_eq!(report.stage, FailureStage::Lint);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].error_class, "lint.clippy_identity_op");
    assert_eq!(
        report.failures[0].target.file_path.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(report.failures[0].target.line, Some(8));
    assert_eq!(report.failures[0].target.column, Some(13));
}

#[test]
fn extracts_machine_usable_targets_from_compile_failures() {
    let stderr = r#"
error[E0560]: struct `Config` has no field named `wat`
 --> src/config.rs:12:9
  |
12 |         wat: true,
  |         ^^^ `Config` does not have this field

error[E0277]: the trait bound `Widget: std::fmt::Display` is not satisfied
 --> src/widget.rs:27:18
  |
27 |     render(widget)
  |     ------ ^^^^^^ the trait `std::fmt::Display` is not implemented for `Widget`
  |
  = help: the trait `Renderer` is implemented for `Widget`
"#;

    let report = parse_cargo_failure(&["cargo".to_string(), "check".to_string()], "", stderr);

    assert_eq!(report.command, CargoCommandKind::Check);
    assert_eq!(report.stage, FailureStage::Compile);
    assert_eq!(report.failures.len(), 2);

    assert_eq!(
        report.failures[0].error_class,
        "compile.unknown_struct_field"
    );
    assert_eq!(
        report.failures[0].target.file_path.as_deref(),
        Some("src/config.rs")
    );
    assert_eq!(report.failures[0].target.line, Some(12));
    assert_eq!(report.failures[0].target.column, Some(9));
    assert_eq!(report.failures[0].target.symbol.as_deref(), Some("Config"));

    assert_eq!(
        report.failures[1].error_class,
        "compile.missing_trait_bound"
    );
    assert_eq!(
        report.failures[1].target.file_path.as_deref(),
        Some("src/widget.rs")
    );
    assert_eq!(report.failures[1].target.line, Some(27));
    assert_eq!(report.failures[1].target.column, Some(18));
    assert_eq!(
        report.failures[1].target.symbol.as_deref(),
        Some("Widget: std::fmt::Display")
    );
    assert!(report.failures[1]
        .message
        .contains("the trait `Renderer` is implemented for `Widget`"));
}

#[test]
fn repo_native_tool_surface_contains_expected_tools() {
    let tools = repo_native_tool_surface();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        vec![
            "read_file",
            "search_symbol",
            "run_tests",
            "apply_patch_temp",
            "git_diff",
            "rollback_candidate"
        ]
    );
}

#[test]
fn parse_command_parts_supports_quotes_and_escapes() {
    let parts =
        parse_command_parts(r#"cargo test -- --exact "suite::quoted name" path\ with\ space"#);
    assert_eq!(
        parts,
        vec![
            "cargo".to_string(),
            "test".to_string(),
            "--".to_string(),
            "--exact".to_string(),
            "suite::quoted name".to_string(),
            "path with space".to_string(),
        ]
    );

    let single_quote_parts = parse_command_parts("cargo test 'suite::single quoted'");
    assert_eq!(
        single_quote_parts,
        vec![
            "cargo".to_string(),
            "test".to_string(),
            "suite::single quoted".to_string(),
        ]
    );
}

#[test]
fn parse_command_parts_stops_at_shell_composition_outside_quotes() {
    let and_parts = parse_command_parts("cargo test && cargo check");
    assert_eq!(and_parts, vec!["cargo".to_string(), "test".to_string()]);

    let pipe_parts = parse_command_parts(r#"cargo test "literal | keep" | cat"#);
    assert_eq!(
        pipe_parts,
        vec![
            "cargo".to_string(),
            "test".to_string(),
            "literal | keep".to_string(),
        ]
    );
}

#[test]
fn detects_shell_composition_tokens_outside_quotes() {
    assert!(contains_shell_composition("cargo test && cargo check"));
    assert!(contains_shell_composition("cargo test | cat"));
    assert!(contains_shell_composition("cargo test ; cargo check"));
    assert!(contains_shell_composition("cargo test > out.txt"));
    assert!(!contains_shell_composition(
        r#"cargo test "literal && still data""#
    ));
    assert!(!contains_shell_composition(
        "cargo test 'literal | still data'"
    ));
}

#[test]
fn classify_cargo_command_handles_env_wrappers_and_assignments() {
    let check = vec![
        "RUSTFLAGS=-Dwarnings".to_string(),
        "env".to_string(),
        "CARGO_TERM_COLOR=never".to_string(),
        "cargo".to_string(),
        "check".to_string(),
    ];
    assert_eq!(classify_cargo_command(&check), CargoCommandKind::Check);

    let clippy = vec![
        "env".to_string(),
        "FOO=bar".to_string(),
        "cargo".to_string(),
        "+nightly".to_string(),
        "clippy".to_string(),
    ];
    assert_eq!(classify_cargo_command(&clippy), CargoCommandKind::Clippy);

    let test = vec![
        "FOO=bar".to_string(),
        "BAR=baz".to_string(),
        "cargo".to_string(),
        "+stable".to_string(),
        "test".to_string(),
        "-p".to_string(),
        "core".to_string(),
    ];
    assert_eq!(classify_cargo_command(&test), CargoCommandKind::Test);

    let not_cargo = vec!["env".to_string(), "FOO=bar".to_string(), "just".to_string()];
    assert_eq!(
        classify_cargo_command(&not_cargo),
        CargoCommandKind::Unknown
    );
}

#[test]
fn classify_cargo_command_handles_parsed_env_prefixed_toolchain_strings() {
    let parts = parse_command_parts(
        "RUST_BACKTRACE=1 env CARGO_TERM_COLOR=never cargo +nightly test --quiet",
    );
    assert_eq!(classify_cargo_command(&parts), CargoCommandKind::Test);

    let check_parts = parse_command_parts("env RUSTFLAGS=-Dwarnings cargo check --workspace");
    assert_eq!(
        classify_cargo_command(&check_parts),
        CargoCommandKind::Check
    );

    let clippy_parts = parse_command_parts("env FOO=bar cargo +stable clippy --quiet");
    assert_eq!(
        classify_cargo_command(&clippy_parts),
        CargoCommandKind::Clippy
    );

    let isolated_clippy_parts =
        parse_command_parts("env -i RUSTFLAGS=-Dwarnings cargo clippy --quiet");
    assert_eq!(
        classify_cargo_command(&isolated_clippy_parts),
        CargoCommandKind::Clippy
    );
}

use std::fs;
use std::path::Path;

use serde_json::Value;

fn fixture(path: &str) -> String {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(repo_root.join(path)).expect("fixture should exist")
}

#[test]
fn planner_schema_fixture_is_versioned_and_closed() {
    let raw = fixture("tests/fixtures/v0_2/planner_response_schema_v1.json");
    let schema: Value = serde_json::from_str(&raw).expect("schema fixture should be valid json");

    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["title"], "Mercury CLI Planner Response v1");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["schema_version"]["const"],
        "planner-response-v1"
    );
    assert!(
        !raw.contains("planner_response_v1"),
        "fixture should use the hyphenated schema version name"
    );

    let required = schema["required"]
        .as_array()
        .expect("top-level required fields should be listed");
    assert_eq!(
        required,
        &vec![
            Value::String("schema_version".into()),
            Value::String("steps".into()),
            Value::String("assessments".into()),
        ]
    );

    assert_eq!(schema["properties"]["steps"]["type"], "array");
    let step = &schema["properties"]["steps"]["items"];
    assert_eq!(step["type"], "object");
    assert_eq!(step["additionalProperties"], false);
    assert_eq!(
        step["required"]
            .as_array()
            .expect("step required fields should be listed"),
        &vec![
            Value::String("file_path".into()),
            Value::String("instruction".into()),
            Value::String("priority".into()),
            Value::String("estimated_tokens".into()),
        ]
    );
    assert_eq!(step["properties"]["file_path"]["minLength"], 1);
    assert_eq!(step["properties"]["estimated_tokens"]["type"], "integer");
    assert_eq!(step["properties"]["estimated_tokens"]["minimum"], 0);
    assert_eq!(step["properties"]["instruction"]["minLength"], 1);
    assert_eq!(step["properties"]["priority"]["minimum"], 0.0);
    assert_eq!(step["properties"]["priority"]["maximum"], 1.0);

    assert_eq!(schema["properties"]["assessments"]["type"], "array");
    let assessment = &schema["properties"]["assessments"]["items"];
    assert_eq!(assessment["type"], "object");
    assert_eq!(assessment["additionalProperties"], false);
    assert_eq!(
        assessment["required"]
            .as_array()
            .expect("assessment required fields should be listed"),
        &vec![
            Value::String("complexity_score".into()),
            Value::String("dependency_score".into()),
            Value::String("risk_score".into()),
            Value::String("churn_score".into()),
            Value::String("suggested_action".into()),
            Value::String("reasoning".into()),
        ]
    );
    assert_eq!(assessment["properties"]["reasoning"]["minLength"], 1);
    assert_eq!(
        assessment["properties"]["suggested_action"]["enum"]
            .as_array()
            .expect("enum should be present")
            .clone(),
        vec![
            Value::String("lock".into()),
            Value::String("refactor".into()),
            Value::String("test".into()),
            Value::String("monitor".into()),
            Value::String("ignore".into()),
        ]
    );
}

#[test]
fn apply_edit_fixture_contains_official_wrapper_sections() {
    let raw = fixture("tests/fixtures/v0_2/mercury_edit_apply_request_v1.txt");
    assert!(raw.contains("<|original_code|>"));
    assert!(raw.contains("<|/original_code|>"));
    assert!(raw.contains("<|update_snippet|>"));
    assert!(raw.contains("<|/update_snippet|>"));
    assert!(raw.contains("{{ORIGINAL_CODE}}"));
    assert!(raw.contains("{{UPDATE_SNIPPET}}"));
    assert!(
        !raw.contains("{{INSTRUCTION}}"),
        "apply-edit fixtures should encode concrete snippets, not instruction placeholders"
    );
}

#[test]
fn next_edit_fixture_documents_required_empty_and_nested_sections() {
    let raw = fixture("tests/fixtures/v0_2/mercury_edit_next_edit_request_v1.txt");
    let recently_viewed_idx = raw
        .find("<|recently_viewed_code_snippets|>")
        .expect("recently viewed wrapper should be present");
    let current_file_idx = raw
        .find("<|current_file_content|>")
        .expect("current file wrapper should be present");
    let history_idx = raw
        .find("<|edit_diff_history|>")
        .expect("diff history wrapper should be present");

    assert!(
        recently_viewed_idx < current_file_idx && current_file_idx < history_idx,
        "next-edit wrapper sections should be ordered viewed -> current file -> history"
    );
    assert!(raw.contains("<|recently_viewed_code_snippets|>"));
    assert!(raw.contains("<|current_file_content|>"));
    assert!(raw.contains("<|code_to_edit|>"));
    assert!(raw.contains("<|cursor|>"));
    assert!(raw.contains("<|edit_diff_history|>"));
    assert!(raw.contains("current_file_path: {{CURRENT_FILE_PATH}}"));
    assert!(raw.contains("{{CURRENT_FILE_CONTENT}}"));
    assert!(raw.contains("{{CODE_TO_EDIT}}"));
    assert!(raw.contains("{{EDIT_HISTORY_UNIDIFF}}"));
    assert!(
        !raw.contains("<|current_file_path|>"),
        "next-edit path should be embedded inside current_file_content"
    );
    assert!(
        !raw.contains("\n<|cursor|>\n{{CURSOR_CONTEXT}}\n<|/cursor|>\n\n<|edit_diff_history|>"),
        "cursor wrapper should stay nested inside code_to_edit"
    );

    let current_file_content = raw
        .split("<|current_file_content|>")
        .nth(1)
        .and_then(|section| section.split("<|/current_file_content|>").next())
        .expect("current_file_content section should be present");
    assert!(
        current_file_content.contains("current_file_path: {{CURRENT_FILE_PATH}}"),
        "current file section must begin with the path line"
    );
    assert!(
        current_file_content.contains("{{CURRENT_FILE_CONTENT}}"),
        "current file section should carry the full file body"
    );

    let code_to_edit = current_file_content
        .split("<|code_to_edit|>")
        .nth(1)
        .and_then(|section| section.split("<|/code_to_edit|>").next())
        .expect("code_to_edit section should be nested inside current_file_content");
    assert!(code_to_edit.contains("{{CODE_TO_EDIT}}"));
    assert!(code_to_edit.contains("<|cursor|>"));
    assert!(code_to_edit.contains("{{CURSOR_CONTEXT}}"));
}

#[test]
#[ignore = "Requires CORE-04 and CORE-05 runtime schema enforcement in src/api.rs and src/engine.rs"]
fn todo_runtime_requests_should_be_checked_against_the_official_fixtures() {
    // TODO(mercury-cli/CORE-05): capture live Mercury Edit apply/next-edit request
    // bodies from MercuryEditClient and compare them against these fixtures.
    let _planner_schema = fixture("tests/fixtures/v0_2/planner_response_schema_v1.json");
    let _apply = fixture("tests/fixtures/v0_2/mercury_edit_apply_request_v1.txt");
    let _next_edit = fixture("tests/fixtures/v0_2/mercury_edit_next_edit_request_v1.txt");
}

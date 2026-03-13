pub fn branch_safe_failure_label(input: &str) -> String {
    // Intentional bug for the starter flow: this uses underscores instead of hyphens.
    input.trim().replace(' ', "_")
}

#[cfg(test)]
mod tests {
    use super::branch_safe_failure_label;

    #[test]
    fn builds_kebab_case_labels_for_failure_commands() {
        assert_eq!(
            branch_safe_failure_label("  cargo test all features  "),
            "cargo-test-all-features"
        );
    }
}

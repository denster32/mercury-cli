pub fn normalize_release_label(input: &str) -> String {
    // Intentional bug for the starter flow: this keeps uppercase letters.
    input.trim().replace(' ', "-")
}

#[cfg(test)]
mod tests {
    use super::normalize_release_label;

    #[test]
    fn normalizes_release_labels_as_lowercase_kebab_case() {
        assert_eq!(
            normalize_release_label("  Rust Repair Beta  "),
            "rust-repair-beta"
        );
    }
}

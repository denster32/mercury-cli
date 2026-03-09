pub fn add(left: i32, right: i32) -> i32 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_expected_total() {
        assert_eq!(add(2, 2), 5);
    }
}

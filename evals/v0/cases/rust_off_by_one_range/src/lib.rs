pub fn count_to(limit: usize) -> usize {
    (0..limit).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_the_upper_bound() {
        assert_eq!(count_to(3), 4);
    }
}

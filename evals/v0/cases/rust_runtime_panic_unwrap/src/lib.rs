pub fn first_value(values: &[i32]) -> i32 {
    values.first().copied().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_on_empty_slice() {
        let _ = first_value(&[]);
    }
}

use std::fmt::Display;

pub fn stringify<T>(value: T) -> String {
    requires_display(value)
}

fn requires_display<T: Display>(value: T) -> String {
    value.to_string()
}

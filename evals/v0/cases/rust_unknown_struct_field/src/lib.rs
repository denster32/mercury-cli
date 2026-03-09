pub struct Counter {
    pub total: usize,
}

pub fn build_counter() -> Counter {
    Counter { count: 1 }
}

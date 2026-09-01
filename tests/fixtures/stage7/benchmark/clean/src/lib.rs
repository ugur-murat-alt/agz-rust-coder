pub fn answer(input: &str) -> Result<usize, &'static str> {
    if input.is_empty() {
        Err("empty input")
    } else {
        Ok(input.len())
    }
}

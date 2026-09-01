pub fn answer() -> u32 {
    42
}

#[cfg(test)]
mod tests {
    use super::answer;

    #[test]
    fn answer_is_stable() {
        assert_eq!(answer(), 42);
    }
}

//! Counts in words, for every message a person reads: "1 device",
//! "2 devices", never "device(s)", which wastes the reader's eye.

/// `n` followed by the word that agrees with it: `one` for exactly one,
/// `many` otherwise (including zero). The words may carry a verb, as in
/// `counted(n, "device holds", "devices hold")`.
pub fn counted(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod tests {
    use super::counted;

    #[test]
    fn one_and_many() {
        assert_eq!(counted(0, "finding", "findings"), "0 findings");
        assert_eq!(counted(1, "finding", "findings"), "1 finding");
        assert_eq!(counted(2, "copy", "copies"), "2 copies");
        assert_eq!(counted(1, "device holds", "devices hold"), "1 device holds");
    }
}

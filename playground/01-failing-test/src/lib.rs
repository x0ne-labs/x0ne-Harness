//! Exercise 01 — a failing test with a real cause.
//!
//! `cargo test` fails. One function is wrong. The tests describe the intended
//! behaviour precisely, so the fix is discoverable without guessing.

/// Split `text` into chunks of at most `size` characters.
///
/// The last chunk may be shorter. `size == 0` yields no chunks rather than
/// looping forever.
pub fn chunk(text: &str, size: usize) -> Vec<String> {
    if size == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let end = (i + size).min(chars.len());
        out.push(chars[i..end].iter().collect());
        // BUG: advancing by `size - 1` overlaps every chunk by one character.
        i += size - 1;
    }
    out
}

/// Sum of the numbers in `values` that are strictly greater than `floor`.
pub fn sum_above(values: &[i64], floor: i64) -> i64 {
    values.iter().filter(|v| **v > floor).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_splits_without_overlap() {
        assert_eq!(chunk("abcdef", 2), vec!["ab", "cd", "ef"]);
    }

    #[test]
    fn chunk_last_piece_may_be_short() {
        assert_eq!(chunk("abcde", 2), vec!["ab", "cd", "e"]);
    }

    #[test]
    fn chunk_size_zero_is_empty_not_infinite() {
        assert!(chunk("abc", 0).is_empty());
    }

    #[test]
    fn chunk_handles_multibyte_characters() {
        // Chunking must count CHARACTERS, not bytes — otherwise this panics
        // or splits a codepoint in half.
        assert_eq!(chunk("héllo", 2), vec!["hé", "ll", "o"]);
    }

    #[test]
    fn sum_above_is_strict() {
        assert_eq!(sum_above(&[1, 5, 10], 5), 10);
        assert_eq!(sum_above(&[], 0), 0);
    }
}

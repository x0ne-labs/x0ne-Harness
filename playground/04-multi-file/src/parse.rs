//! Parsing amounts out of text.

use crate::money::Money;

/// Parse `"12.34"` into cents. Returns `None` on anything malformed.
pub fn parse_amount(s: &str) -> Option<Money> {
    let s = s.trim();
    match s.split_once('.') {
        Some((eur, cents)) => {
            if cents.len() != 2 {
                return None;
            }
            let eur: i64 = eur.parse().ok()?;
            let cents: i64 = cents.parse().ok()?;
            Some(eur * 100 + cents)
        }
        None => {
            let eur: i64 = s.parse().ok()?;
            Some(eur * 100)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_euros_and_cents() {
        assert_eq!(parse_amount("12.34"), Some(1234));
    }

    #[test]
    fn parses_a_bare_integer_as_euros() {
        // The unit trap this exercise is about: a bare "7" means SEVEN EUROS,
        // not seven cents. Nothing in the type stops the next reader getting
        // this backwards.
        assert_eq!(parse_amount("7"), Some(700));
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(parse_amount("12.3"), None);
        assert_eq!(parse_amount("abc"), None);
    }
}

//! Amounts. Everything here is CENTS — a fact the types do not currently say.

/// An amount of money, in cents.
pub type Money = i64;

/// Euros to the internal unit.
pub fn from_euros(euros: i64) -> Money {
    euros * 100
}

/// Add two amounts.
pub fn add(a: Money, b: Money) -> Money {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn euros_become_cents() {
        assert_eq!(from_euros(3), 300);
    }

    #[test]
    fn addition_stays_in_cents() {
        assert_eq!(add(from_euros(1), 50), 150);
    }
}

//! Rendering amounts for a human.

use crate::money::{add, Money};

/// Render an amount as `"12.34 €"`.
pub fn format_amount(m: Money) -> String {
    format!("{}.{:02} €", m / 100, (m % 100).abs())
}

/// Sum and render.
pub fn format_total(amounts: &[Money]) -> String {
    let total = amounts.iter().copied().fold(0, add);
    format_amount(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::from_euros;

    #[test]
    fn formats_with_two_decimals() {
        assert_eq!(format_amount(1234), "12.34 €");
        assert_eq!(format_amount(700), "7.00 €");
    }

    #[test]
    fn totals_a_list() {
        assert_eq!(format_total(&[from_euros(1), 250]), "3.50 €");
    }
}

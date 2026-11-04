use toy::parser::{parse_all, parse_line, parse_number};

#[test]
fn test_parse_line_splits_on_colon() {
    assert_eq!(parse_line("k: v"), ("k".into(), "v".into()));
}

#[test]
fn test_parse_all_counts_lines() {
    assert_eq!(parse_all("a: 1\nb: 2").len(), 2);
}

#[test]
fn test_parse_number_trims() {
    assert_eq!(parse_number(" 42 "), 42);
}

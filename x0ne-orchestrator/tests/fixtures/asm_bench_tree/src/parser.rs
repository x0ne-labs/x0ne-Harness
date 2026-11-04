/// Parses "key: value" lines.
pub fn parse_line(line: &str) -> (String, String) {
    let idx = line.find(':').unwrap();
    (line[..idx].to_string(), line[idx + 1..].trim().to_string())
}

pub fn parse_all(text: &str) -> Vec<(String, String)> {
    text.lines().map(parse_line).collect()
}

// FIXME: handle escaped colons
pub fn parse_number(s: &str) -> i64 {
    s.trim().parse().unwrap()
}

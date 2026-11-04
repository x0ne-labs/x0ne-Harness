//! Adversarial probe: `parse_iso8601_unix` slices `&s[19..]` directly. That is
//! safe only because `num(17, 19)` must have succeeded first, and `str::get`
//! refuses a non-boundary index. This test is the proof of that reasoning
//! rather than the reasoning itself.
#[test]
fn no_input_makes_the_timestamp_parser_panic() {
    let alphabet = [
        '0',
        '9',
        '-',
        'T',
        ':',
        '.',
        '+',
        'Z',
        'é',
        '€',
        '\u{10348}',
        ' ',
    ];
    let mut rng: u64 = 0x2026_0824;
    for _ in 0..200_000 {
        let mut s = String::new();
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (rng >> 33) as usize % 30;
        for _ in 0..len {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s.push(alphabet[(rng >> 33) as usize % alphabet.len()]);
        }
        // Must return, never panic.
        let _ = x0ne_cloud::spend::parse_iso8601_unix(&s);
    }
    // And the shapes closest to the boundary, explicitly.
    for s in [
        "2026-08-20T09:00:0é",  // byte 19 mid-char
        "2026-08-20T09:00:00é", // byte 19 starts a char
        "2026-08-20T09:00:00",  // exactly 19 bytes, no tail
        "2026-08-20T09:00:00\u{10348}",
        "202é-08-20T09:00:00Z",
    ] {
        let _ = x0ne_cloud::spend::parse_iso8601_unix(s);
    }
}

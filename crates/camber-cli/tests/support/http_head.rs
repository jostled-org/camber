//! Response-head parsing shared by every test client.

/// The status code on the start line of a response `head`.
pub fn status_code(head: &str) -> Option<u16> {
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
}

/// The trimmed value of the first `name` header in `head`, matched without
/// case. The start line is skipped.
pub fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field
            .trim()
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

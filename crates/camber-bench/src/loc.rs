use std::path::Path;

/// Count non-blank, non-comment lines in a source file.
/// Handles C-style comments (`//`, `/* */`) used by Rust, Go, and similar languages.
pub fn count_source_loc(path: &Path) -> std::io::Result<usize> {
    let content = std::fs::read_to_string(path)?;
    Ok(count_loc(&content))
}

/// Count non-blank, non-comment lines in a source string.
pub fn count_loc(source: &str) -> usize {
    let mut in_block_comment = false;
    let mut count = 0;

    for line in source.lines() {
        if line_has_code(line, &mut in_block_comment) {
            count += 1;
        }
    }

    count
}

fn line_has_code(line: &str, in_block_comment: &mut bool) -> bool {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut has_code = false;

    while index < bytes.len() {
        match (*in_block_comment, starts_with(bytes, index, b"*/")) {
            (true, true) => {
                *in_block_comment = false;
                index += 2;
                continue;
            }
            (true, false) => {
                index += 1;
                continue;
            }
            (false, _) => {}
        }

        if starts_with(bytes, index, b"//") {
            return has_code;
        }

        if starts_with(bytes, index, b"/*") {
            *in_block_comment = true;
            index += 2;
            continue;
        }

        if !bytes[index].is_ascii_whitespace() {
            has_code = true;
        }

        index += 1;
    }

    has_code
}

fn starts_with(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes.get(index..index + needle.len()) == Some(needle)
}

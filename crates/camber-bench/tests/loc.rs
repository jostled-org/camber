use camber_bench::loc::count_loc;

#[test]
fn counts_non_blank_non_comment() {
    let source = r#"
// This is a comment
use std::io;

fn main() {
    println!("hello");
}

/* block comment */
"#;
    assert_eq!(count_loc(source), 4);
}

#[test]
fn counts_lines_with_inline_block_comments() {
    let source = r#"
let alpha = 1; /* explain alpha */
/* preface */ let beta = 2;
let gamma = /* inline */ 3;
"#;

    assert_eq!(count_loc(source), 3);
}

#[test]
fn ignores_comment_only_segments_inside_mixed_block_comments() {
    let source = r#"
let alpha = 1;
/* comment starts
still comment */ let beta = 2;
let gamma = 3; // trailing comment
"#;

    assert_eq!(count_loc(source), 3);
}

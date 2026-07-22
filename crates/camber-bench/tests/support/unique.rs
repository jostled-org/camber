use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

pub fn name(prefix: &str) -> Box<str> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let prefix: String = prefix
        .chars()
        .map(|character| match character.is_ascii_alphanumeric() {
            true => character,
            false => '-',
        })
        .collect();
    format!("{prefix}-{}-{nanos}-{sequence}", std::process::id()).into_boxed_str()
}

pub fn external_resource(prefix: &str) -> Box<str> {
    name(prefix)
}

pub fn build(prefix: &str) -> Box<str> {
    name(prefix)
}

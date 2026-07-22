use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static RESOURCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn unique_external_name(prefix: &str) -> Box<str> {
    let prefix: String = prefix
        .chars()
        .map(|character| match character.is_ascii_alphanumeric() {
            true => character.to_ascii_lowercase(),
            false => '-',
        })
        .collect();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = RESOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{timestamp}-{sequence}", std::process::id()).into_boxed_str()
}

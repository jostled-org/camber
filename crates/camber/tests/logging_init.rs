use camber::logging::{LogFormat, LogLevel, init_logging};

/// Setting the global subscriber can only happen once per process.
/// These tests run in the same process, so only the first `init_logging`
/// call installs a subscriber; subsequent calls silently fail.
/// We verify that neither call panics regardless of ordering.

#[test]
fn init_logging_text_format_does_not_panic() {
    init_logging(LogFormat::Text, LogLevel::Info);
}

#[test]
fn init_logging_json_format_does_not_panic() {
    init_logging(LogFormat::Json, LogLevel::Info);
}

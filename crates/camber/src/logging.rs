use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Output format for the tracing subscriber.
#[derive(Debug, Clone, Copy)]
pub enum LogFormat {
    Text,
    Json,
}

/// Verbosity level for the tracing subscriber.
#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_filter(self) -> tracing_subscriber::filter::LevelFilter {
        match self {
            Self::Error => tracing_subscriber::filter::LevelFilter::ERROR,
            Self::Warn => tracing_subscriber::filter::LevelFilter::WARN,
            Self::Info => tracing_subscriber::filter::LevelFilter::INFO,
            Self::Debug => tracing_subscriber::filter::LevelFilter::DEBUG,
            Self::Trace => tracing_subscriber::filter::LevelFilter::TRACE,
        }
    }
}

/// Initialize the global tracing subscriber.
///
/// Installs a subscriber with the given format and level filter.
/// If a global subscriber is already set, this is a no-op.
pub fn init_logging(format: LogFormat, level: LogLevel) {
    let filter = level.as_filter();
    let registry = tracing_subscriber::registry().with(filter);

    let result = match format {
        LogFormat::Text => registry.with(fmt::layer()).try_init(),
        LogFormat::Json => registry.with(fmt::layer().json()).try_init(),
    };

    // Silently ignore if a subscriber is already installed.
    drop(result);
}

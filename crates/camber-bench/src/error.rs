use std::io;

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid benchmark configuration: {0}")]
    InvalidConfig(Box<str>),

    #[error("http error: {0}")]
    Http(Box<str>),

    #[error("server failed to start: {0}")]
    ServerStart(Box<str>),

    #[error("load generator error: {0}")]
    LoadGenerator(Box<str>),
}

impl From<reqwest::Error> for BenchError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string().into_boxed_str())
    }
}

impl From<std::sync::mpsc::RecvError> for BenchError {
    fn from(e: std::sync::mpsc::RecvError) -> Self {
        Self::ServerStart(e.to_string().into_boxed_str())
    }
}

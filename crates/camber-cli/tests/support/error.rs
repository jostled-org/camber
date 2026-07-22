use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub struct FixtureError(Box<str>);

impl FixtureError {
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self(message.into())
    }
}

impl Display for FixtureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FixtureError {}

impl From<std::io::Error> for FixtureError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<String> for FixtureError {
    fn from(error: String) -> Self {
        Self::new(error)
    }
}

impl From<Box<str>> for FixtureError {
    fn from(error: Box<str>) -> Self {
        Self(error)
    }
}

impl From<std::string::FromUtf8Error> for FixtureError {
    fn from(error: std::string::FromUtf8Error) -> Self {
        Self::new(error.to_string())
    }
}

use std::{fmt, io};

#[derive(Debug, Clone)]
pub struct Error(pub String);

impl Error {
    pub fn open(path: impl fmt::Display, reason: impl fmt::Display) -> Self {
        Self(format!("cannot open {path}: {reason}"))
    }

    pub fn cache(reason: impl fmt::Display) -> Self {
        Self(format!("cache: {reason}"))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self(value.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self(value.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

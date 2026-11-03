use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Reading the file failed.
    Io(String),
    /// The file is damaged, cut short or not what it claims to be.
    Corrupt(String),
    /// A valid file that uses something this crate doesn't read yet.
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn corrupt(msg: impl Into<String>) -> Self {
        Error::Corrupt(msg.into())
    }

    pub(crate) fn unsupported(what: impl Into<String>) -> Self {
        Error::Unsupported(what.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(msg) => write!(f, "reading the file failed: {msg}"),
            Error::Corrupt(msg) => write!(f, "corrupt video: {msg}"),
            Error::Unsupported(what) => {
                write!(f, "the video uses {what}, which black-hevc can't read yet")
            }
        }
    }
}

impl std::error::Error for Error {}

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("nix: {0}")]
    Nix(#[from] nix::Error),
    #[error("{0}")]
    Msg(String),
    #[error("channel {0:?} not found")]
    ChannelNotFound(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("frontend did not lock before timeout")]
    LockTimeout,
    #[error("SI parse: {0}")]
    Si(String),
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Msg(s.to_string())
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Msg(s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("iroh error: {0}")]
    Iroh(#[from] anyhow::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid ticket: {0}")]
    InvalidTicket(String),

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("transfer failed: {0}")]
    TransferFailed(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("timeout")]
    Timeout,

    #[error("cancelled")]
    Cancelled,
}

impl From<iroh::endpoint::ConnectionError> for Error {
    fn from(e: iroh::endpoint::ConnectionError) -> Self {
        Error::ConnectionFailed(e.to_string())
    }
}

impl From<iroh::endpoint::ConnectingError> for Error {
    fn from(e: iroh::endpoint::ConnectingError) -> Self {
        Error::ConnectionFailed(e.to_string())
    }
}

impl From<iroh::endpoint::ClosedStream> for Error {
    fn from(e: iroh::endpoint::ClosedStream) -> Self {
        Error::TransferFailed(e.to_string())
    }
}

impl From<iroh::endpoint::WriteError> for Error {
    fn from(e: iroh::endpoint::WriteError) -> Self {
        Error::TransferFailed(e.to_string())
    }
}

impl From<iroh::endpoint::ReadExactError> for Error {
    fn from(e: iroh::endpoint::ReadExactError) -> Self {
        Error::TransferFailed(e.to_string())
    }
}

impl From<iroh::endpoint::BindError> for Error {
    fn from(e: iroh::endpoint::BindError) -> Self {
        Error::ConnectionFailed(e.to_string())
    }
}

impl From<iroh::endpoint::ConnectError> for Error {
    fn from(e: iroh::endpoint::ConnectError) -> Self {
        Error::ConnectionFailed(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

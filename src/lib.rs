#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::linkedlist)]
#![doc = include_str!("../README.md")]

use std::sync::Arc;

mod call;
mod header;
mod packet;
pub use call::Call;
pub use header::{Header, HEADER_LEN};
pub use packet::{Packet, Pid, Port};

#[cfg(feature = "crypto")]
pub mod crypto;

pub mod wrap;

mod v1;
pub use v1::*;
pub mod v2;

pub mod r#async;
pub mod proxy;

#[cfg(feature = "native")]
pub mod native;

#[derive(thiserror::Error, Debug, Clone)]
pub enum Error {
    /// An error with only a plain text message.
    #[error("An error occurred: {0}")]
    Msg(String),

    /// An IO error without a known file associated.
    #[error("IO Error: {0}")]
    Io(std::sync::Arc<std::io::Error>),

    #[error("Lock poisoned")]
    PoisonError,

    #[error("From int error")]
    IntConvert(#[from] std::num::TryFromIntError),

    /// A wrapper around another error.
    #[error("{msg:?}: {source:?}")]
    Other {
        #[source]
        source: Arc<dyn std::error::Error + Send + Sync>,
        msg: Option<String>,
    },
}

impl<T> From<std::sync::PoisonError<T>> for Error {
    fn from(_: std::sync::PoisonError<T>) -> Self {
        Error::PoisonError
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(std::sync::Arc::new(e))
    }
}

impl Error {
    pub fn msg<T: Into<String>>(m: T) -> Error {
        Error::Msg(m.into())
    }
    fn other(e: impl std::error::Error + Send + Sync + 'static) -> Error {
        Error::Other {
            source: Arc::new(e),
            msg: None,
        }
    }
}

/// Result convenience type.
pub type Result<T> = std::result::Result<T, Error>;

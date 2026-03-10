#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::borrow_as_ptr)]
#![allow(clippy::ref_as_ptr)]
#![allow(clippy::linkedlist)]
#![doc = include_str!("../README.md")]
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

pub mod r#async;
pub mod proxy;

#[cfg(feature = "native")]
pub mod native;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// An error with only a plain text message.
    #[error("An error occurred: {0}")]
    Msg(String),

    /// An IO error without a known file associated.
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),

    /// A wrapper around another error.
    #[error("{msg:?}: {source:?}")]
    Other {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
        msg: Option<String>,
    },
}

impl Error {
    fn msg<T: Into<String>>(m: T) -> Error {
        Error::Msg(m.into())
    }
    fn other(e: impl std::error::Error + Send + Sync + 'static) -> Error {
        Error::Other {
            source: Box::new(e),
            msg: None,
        }
    }
}

/// Result convenience type.
pub type Result<T> = std::result::Result<T, Error>;

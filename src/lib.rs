//#![allow(clippy::doc_markdown)]
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
    Plain(String),

    /// A wrapper around another error.
    #[error("{msg:?}: {source:?}")]
    Other {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
        msg: Option<String>,
    },
}

/// Result convenience type.
pub type Result<T> = std::result::Result<T, Error>;

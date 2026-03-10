use std::io::{Read, Write};

use crate::Result;

/// An interface to wrap data, e.g. for signed packet sending.
pub trait Wrapper {
    /// Wrap a value.
    ///
    /// # Errors
    ///
    /// Implementation defined.
    fn wrap(&self, input: &[u8]) -> Result<Vec<u8>>;
    /// Wrap a value.
    ///
    /// # Errors
    ///
    /// Implementation defined. But in the case of signed, if the signature
    /// doesn't match.
    fn unwrap(&self, input: &[u8]) -> Result<Vec<u8>>;
}

pub struct Wrap<T: Read + Write, W: Wrapper> {
    backend: T,
    wrapper: W,
}

impl<T: Read + Write, W: Wrapper> Wrap<T, W> {
    pub fn new(backend: T, wrapper: W) -> Self {
        Self { backend, wrapper }
    }
    pub fn into_inner(self) -> (T, W) {
        (self.backend, self.wrapper)
    }
}

impl<T: Read + Write, W: Wrapper> Read for Wrap<T, W> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = self.backend.read(buf)?;
        let buf2 = &buf[..size];
        let msg = self
            .wrapper
            .wrap(buf2)
            .map_err(|e| std::io::Error::other(format!("{e}")))?;
        let msglen = msg.len();
        buf.copy_from_slice(&msg);
        Ok(msglen)
    }
}

impl<T: Read + Write, W: Wrapper> Write for Wrap<T, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.backend.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.backend.flush()
    }
}

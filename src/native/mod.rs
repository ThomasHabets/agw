use libc::c_void;
use std::io::{Read, Write};

use crate::{Error, Result};

type BinaryCall = [u8; 7];

fn empty_call() -> BinaryCall {
    [0u8; 7]
}

/// This is the same as ax25_aton_entry(), but spares us depending on libax25.
pub fn parse_call(call: &str) -> Result<BinaryCall> {
    let empty = b' ' << 1;
    let mut bin = [empty; 7];
    let split: Vec<_> = call.splitn(2, '-').collect();
    let (call, ssid) = match split.len() {
        1 => (split[0], 0),
        2 => (split[0], split[1].parse::<u8>().map_err(Error::other)?),
        _ => panic!(),
    };
    if ssid > 15 {
        return Err(Error::msg("SSID out of range in {call}"));
    }
    if call.len() < 3 || call.len() > 6 {
        return Err(Error::msg("SSID out of range in {call}"));
    }
    for (i, ch) in call.chars().enumerate() {
        if !ch.is_ascii_alphanumeric() {
            return Err(Error::msg("non-alphanum in {call}"));
        }
        let ch = ch
            .to_uppercase()
            .next()
            .expect("internal error: character can't be uppercased");
        bin[i] = (ch as u8) << 1;
    }
    bin[6] = ssid << 1;
    Ok(bin)
}

struct FD {
    fd: i32,
}
impl FD {
    fn new(fd: i32) -> FD {
        FD { fd }
    }
    fn get(&self) -> Option<i32> {
        if self.fd >= 0 {
            Some(self.fd)
        } else {
            None
        }
    }
    fn close(&mut self) {
        if self.fd >= 0 {
            // TODO: check close error.
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}
impl Drop for FD {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct NativeStream {
    fd: FD,
}

/// This is exactly the Linux libax25 C equivalent struct.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct full_sockaddr_ax25 {
    pub sax25_family: libc::sa_family_t,
    pub sax25_call: BinaryCall,
    pub sax25_ndigis: libc::c_int,
    pub sax25_digipeater: [BinaryCall; 8], // max 8 digipeaters
}

mod primitive {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    pub fn socket() -> Result<FD> {
        let fd = FD::new(unsafe { libc::socket(libc::AF_AX25, libc::SOCK_SEQPACKET, 0) });
        fd.get().ok_or(std::io::Error::last_os_error())?;
        Ok(fd)
    }
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn make_sa(call: &BinaryCall, digis: &[BinaryCall]) -> Result<full_sockaddr_ax25> {
        let mut sax25_digipeater = [empty_call(); 8];
        for (i, digi) in digis.iter().enumerate() {
            if i >= sax25_digipeater.len() {
                // TODO: return error?
                break;
            }
            sax25_digipeater[i] = *digi;
        }
        Ok(full_sockaddr_ax25 {
            sax25_family: libc::sa_family_t::try_from(libc::AF_AX25).expect("can't happen"),
            sax25_call: *call,
            sax25_ndigis: libc::c_int::try_from(digis.len()).map_err(Error::other)?,
            sax25_digipeater,
        })
    }

    pub fn bind(fd: &FD, mycall: &BinaryCall, digis: &[BinaryCall]) -> Result<()> {
        let sa = make_sa(mycall, digis)?;
        let sa_ptr = (&raw const sa).cast::<libc::sockaddr>();
        let rc = unsafe {
            libc::bind(
                fd.get().ok_or(std::io::Error::last_os_error())?,
                sa_ptr,
                u32::try_from(std::mem::size_of::<full_sockaddr_ax25>()).map_err(Error::other)?,
            )
        };
        if rc == -1 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }
    /*    pub fn ax25_aton_entry(call: &str) -> Result<BinaryCall> {
            let mut sax25_call = [0u8; 7];
            let rc = unsafe {
                native::ax25_aton_entry(
                    CString::new(call)?.as_ptr(),
                    &mut sax25_call as *mut libc::c_uchar,
                )
            };
            if rc == -1 {
                Err(anyhow::Error::msg("failed to parse call {call}"))
            } else {
                Ok(sax25_call)
            }
    }
        */
    pub fn connect(fd: &FD, call: &BinaryCall, digis: &[BinaryCall]) -> Result<()> {
        let sa = make_sa(call, digis)?;
        let sa_ptr = (&raw const sa).cast::<libc::sockaddr>();
        if -1
            == unsafe {
                libc::connect(
                    fd.get()
                        .ok_or(Error::msg("calling connect() with invalid socket"))?,
                    sa_ptr,
                    u32::try_from(std::mem::size_of::<full_sockaddr_ax25>())
                        .map_err(Error::other)?,
                )
            }
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub fn read(fd: &FD, buf: &mut [u8]) -> Result<usize> {
        let fd = fd
            .get()
            .ok_or(std::io::Error::other("read() called on closed socket"))?;
        let rc = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<c_void>(), buf.len()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        usize::try_from(rc).map_err(Error::other)
    }
    pub fn write(fd: &FD, buf: &[u8]) -> Result<usize> {
        let fd = fd
            .get()
            .ok_or(std::io::Error::other("write() called on closed socket"))?;
        let rc = unsafe { libc::write(fd, buf.as_ptr().cast::<c_void>(), buf.len()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        usize::try_from(rc).map_err(Error::other)
    }
}

impl NativeStream {
    pub fn connect(
        mycall: &BinaryCall,
        radio: &BinaryCall,
        call: &BinaryCall,
        digis: &[BinaryCall],
    ) -> Result<Self> {
        let fd = primitive::socket().map_err(Error::other)?;
        primitive::bind(&fd, mycall, &[*radio]).map_err(Error::other)?;
        primitive::connect(&fd, call, digis).map_err(Error::other)?;
        Ok(Self { fd })
    }
}

impl Read for NativeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        primitive::read(&self.fd, buf).map_err(std::io::Error::other)
    }
}

impl Write for NativeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        primitive::write(&self.fd, buf).map_err(std::io::Error::other)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub trait Stream: Read + Write {}
impl Stream for NativeStream {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_good_calls() -> Result<()> {
        for (call, want) in [
            ("W2B", [174u8, 100, 132, 64, 64, 64, 0]),
            ("M0THC2", [154u8, 96, 168, 144, 134, 100, 0]),
            ("M0THC2-3", [154u8, 96, 168, 144, 134, 100, 6]),
            ("M0THC2-15", [154u8, 96, 168, 144, 134, 100, 30]),
            ("M0THC", [154u8, 96, 168, 144, 134, 64, 0]),
            ("M0THC-0", [154u8, 96, 168, 144, 134, 64, 0]),
            ("M0THC-1", [154u8, 96, 168, 144, 134, 64, 2]),
            ("m0thc-1", [154u8, 96, 168, 144, 134, 64, 2]),
            ("M0THC-2", [154u8, 96, 168, 144, 134, 64, 4]),
            ("M0THC-15", [154u8, 96, 168, 144, 134, 64, 30]),
        ] {
            assert_eq!(want, parse_call(call)?, "failed for {call}");
        }
        Ok(())
    }

    #[test]
    fn parse_bad_calls() {
        for call in [
            "",
            "M",
            "M0",
            "-1",
            "toolongcall",
            "M0THC-16",
            "M0THC-22",
            "M0THC15",
            "M0THC-",
            "M0THC…",
        ] {
            if let Ok(v) = parse_call(call) {
                panic!("succeeded for {call} into {v:?}, should fail");
            }
        }
    }
}

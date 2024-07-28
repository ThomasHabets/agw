use anyhow::Result;
use libc::c_void;
use std::ffi::CString;
use std::io::{Error, ErrorKind, Read, Write};

mod native {
    #[link(name = "ax25", kind = "dylib")]
    extern "C" {
        // This is the C function you want to call
        pub fn ax25_aton_entry(cp: *const libc::c_char, axp: *mut libc::c_uchar) -> libc::c_int;
    }
}

type BinaryCall = [u8; 7];

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

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct full_sockaddr_ax25 {
    pub sax25_family: libc::sa_family_t,
    pub sax25_call: [u8; 7],
    pub sax25_ndigis: libc::c_int,
    pub sax25_digipeater: [[u8; 7]; 8], // max 8 digipeaters
}

mod primitive {
    use super::*;
    use anyhow::Result;
    pub fn socket() -> Result<FD> {
        let fd = FD::new(unsafe { libc::socket(libc::AF_AX25, libc::SOCK_SEQPACKET, 0) });
        fd.get().ok_or(std::io::Error::last_os_error())?;
        Ok(fd)
    }
    pub fn make_sa(call: &str, digis: &[&str]) -> Result<full_sockaddr_ax25> {
        let mut sax25_digipeater = [[0u8; 7]; 8];
        for (i, digi) in digis.iter().enumerate() {
            if i >= sax25_digipeater.len() {
                // TODO: return error?
                break;
            }
            sax25_digipeater[i] = primitive::ax25_aton_entry(*digi)?;
        }
        Ok(full_sockaddr_ax25 {
            sax25_family: libc::AF_AX25 as libc::sa_family_t,
            sax25_call: ax25_aton_entry(call)?,
            sax25_ndigis: digis.len() as libc::c_int,
            sax25_digipeater,
        })
    }

    pub fn bind(fd: &FD, mycall: &str, digis: &[&str]) -> Result<()> {
        let sa = make_sa(mycall, digis)?;
        let sa_ptr = &sa as *const _ as *const libc::sockaddr;
        let rc = unsafe {
            libc::bind(
                fd.get().ok_or(std::io::Error::last_os_error())?,
                sa_ptr,
                std::mem::size_of::<full_sockaddr_ax25>() as u32,
            )
        };
        if rc == -1 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }
    pub fn ax25_aton_entry(call: &str) -> Result<BinaryCall> {
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
    pub fn connect(fd: &FD, call: &str, digis: &[&str]) -> Result<()> {
        let sa = make_sa(call, digis)?;
        let sa_ptr = &sa as *const _ as *const libc::sockaddr;
        if -1
            == unsafe {
                libc::connect(
                    fd.get()
                        .ok_or(anyhow::Error::msg("calling connect() with invalid socket"))?,
                    sa_ptr,
                    std::mem::size_of::<full_sockaddr_ax25>() as u32,
                )
            }
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub fn read(fd: &FD, buf: &mut [u8]) -> std::io::Result<usize> {
        let fd = fd.get().ok_or(Error::new(
            ErrorKind::Other,
            "read() called on closed socket",
        ))?;
        let rc = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(rc as usize)
    }
    pub fn write(fd: &FD, buf: &[u8]) -> std::io::Result<usize> {
        let fd = fd.get().ok_or(Error::new(
            ErrorKind::Other,
            "write() called on closed socket",
        ))?;
        let rc = unsafe { libc::write(fd, buf.as_ptr() as *const c_void, buf.len()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(rc as usize)
    }
}

impl NativeStream {
    pub fn connect(mycall: &str, radio: &str, call: &str, digis: &[&str]) -> Result<Self> {
        let fd = primitive::socket()?;
        primitive::bind(&fd, mycall, &[radio])?;
        primitive::connect(&fd, call, digis)?;
        Ok(Self { fd })
    }
}

impl Read for NativeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        primitive::read(&self.fd, buf)
    }
}

impl Write for NativeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        primitive::write(&self.fd, buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub trait Stream: Read + Write {}
impl Stream for NativeStream {}

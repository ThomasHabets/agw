use anyhow::Result;
use log::error;

extern crate libc;

pub struct PubKey {
    pubkey: Vec<u8>,
}

pub struct SecKey {
    seckey: Vec<u8>,
}

impl PubKey {
    fn new() -> Self {
        PubKey {
            pubkey: vec![0; unsafe { crypto_sign_publickeybytes() } as usize],
        }
    }
    pub fn load(fname: &std::path::Path) -> Result<PubKey> {
        // TODO: don't OOM if we point to the wrong place.
        let pubkey = std::fs::read(fname)?;
        if pubkey.len() != unsafe { crypto_sign_publickeybytes() } as usize {
            return Err(anyhow::Error::msg("public key file has wrong size"));
        }
        Ok(PubKey { pubkey })
    }
    fn as_mut_ptr(&mut self) -> *mut libc::c_uchar {
        self.pubkey.as_mut_ptr()
    }
    fn as_ptr(&self) -> *const libc::c_uchar {
        self.pubkey.as_ptr()
    }
}
impl SecKey {
    fn new() -> Self {
        SecKey {
            seckey: vec![0; unsafe { crypto_sign_secretkeybytes() } as usize],
        }
    }
    pub fn load(fname: &std::path::Path) -> Result<SecKey> {
        // TODO: don't OOM if we point to the wrong place.
        let seckey = std::fs::read(fname)?;
        if seckey.len() != unsafe { crypto_sign_secretkeybytes() } as usize {
            return Err(anyhow::Error::msg("secret key file has wrong size"));
        }
        Ok(SecKey { seckey })
    }
    fn as_mut_ptr(&mut self) -> *mut libc::c_uchar {
        self.seckey.as_mut_ptr()
    }
    fn as_ptr(&self) -> *const libc::c_uchar {
        self.seckey.as_ptr()
    }
}

#[link(name = "sodium", kind = "dylib")]
extern "C" {
    fn sodium_init();
    fn crypto_sign(
        sm: *mut libc::c_uchar,
        smlen: *mut libc::c_ulonglong,
        msg: *const libc::c_uchar,
        msglen: libc::c_ulonglong,
        sk: *const libc::c_uchar,
    ) -> libc::c_int;
    fn crypto_sign_detached(
        sig: *mut libc::c_uchar,
        siglen: *mut libc::c_ulonglong,
        msg: *const libc::c_uchar,
        msglen: libc::c_ulonglong,
        sk: *const libc::c_uchar,
    ) -> libc::c_int;
    fn crypto_sign_open(
        msg: *mut libc::c_uchar,
        msglen: *mut libc::c_ulonglong,
        sm: *const libc::c_uchar,
        smlen: libc::c_ulonglong,
        pubkey: *const libc::c_uchar,
    ) -> libc::c_int;
    fn crypto_sign_verify_detached(
        sig: *const libc::c_uchar,
        msg: *const libc::c_uchar,
        msglen: libc::c_ulonglong,
        pubkey: *const libc::c_uchar,
    ) -> libc::c_int;
    fn crypto_sign_keypair(pubkey: *mut libc::c_uchar, sk: *mut libc::c_uchar) -> libc::c_int;
    fn crypto_sign_publickeybytes() -> libc::c_ulonglong;
    fn crypto_sign_secretkeybytes() -> libc::c_ulonglong;
    fn crypto_sign_bytes() -> libc::c_ulonglong;
}

fn init() {
    unsafe {
        sodium_init();
    }
}

pub fn sign(msg: &[u8], key: &SecKey) -> Result<Vec<u8>> {
    init();
    let mut sig = vec![0u8; msg.len() + unsafe { crypto_sign_bytes() } as usize];
    // siglen is actually a strict out parameter. But in case that changes,
    // let's set it.
    let mut siglen: libc::c_ulonglong = sig.len().try_into()?;
    let rc = unsafe {
        crypto_sign(
            sig.as_mut_ptr(),
            &mut siglen as *mut _,
            msg.as_ptr(),
            msg.len() as libc::c_ulonglong,
            key.as_ptr(),
        )
    };
    if rc == -1 {
        Err(anyhow::Error::msg("crypto_sign_detached() failed"))
    } else {
        Ok(sig[..(siglen as usize)].to_vec())
    }
}

pub fn sign_detached(msg: &[u8], key: &SecKey) -> Result<Vec<u8>> {
    init();
    let mut sig = vec![0u8; unsafe { crypto_sign_bytes() } as usize];
    // siglen is actually a strict out parameter. But in case that changes,
    // let's set it.
    let mut siglen: libc::c_ulonglong = sig.len().try_into()?;
    let rc = unsafe {
        crypto_sign_detached(
            sig.as_mut_ptr(),
            &mut siglen as *mut _,
            msg.as_ptr(),
            msg.len() as libc::c_ulonglong,
            key.as_ptr(),
        )
    };
    assert_eq!(siglen, unsafe { crypto_sign_bytes() });
    if rc == -1 {
        Err(anyhow::Error::msg("crypto_sign_detached() failed"))
    } else {
        Ok(sig[..(siglen as usize)].to_vec())
    }
}

pub fn open(sig: &[u8], pubkey: &PubKey) -> Option<Vec<u8>> {
    init();
    let siglen = sig.len();
    let rightlen = unsafe { crypto_sign_bytes() } as usize;
    if siglen < rightlen {
        error!("Signature length incorrect: expected {siglen} >= {rightlen}");
        return None;
    }
    let mut msg = vec![0u8; siglen - rightlen];
    let mut msglen: libc::c_ulonglong = 0;
    let rc = unsafe {
        crypto_sign_open(
            msg.as_mut_ptr(),
            &mut msglen as *mut libc::c_ulonglong,
            sig.as_ptr(),
            siglen as libc::c_ulonglong,
            pubkey.as_ptr(),
        )
    };
    if rc == 0 {
        Some(msg)
    } else {
        None
    }
}

pub fn verify_detached(sig: &[u8], msg: &[u8], pubkey: &PubKey) -> bool {
    init();
    let siglen = sig.len();
    let rightlen = unsafe { crypto_sign_bytes() } as usize;
    if siglen != rightlen {
        error!("Signature length incorrect: expected {rightlen} got {siglen}");
        return false;
    }
    let rc = unsafe {
        crypto_sign_verify_detached(
            sig.as_ptr(),
            msg.as_ptr(),
            msg.len() as libc::c_ulonglong,
            pubkey.as_ptr(),
        )
    };
    rc == 0
}

pub fn keygen() -> Result<(PubKey, SecKey)> {
    init();
    let mut pk = PubKey::new();
    let mut sk = SecKey::new();
    assert_eq!(0, unsafe {
        crypto_sign_keypair(pk.as_mut_ptr(), sk.as_mut_ptr())
    });
    Ok((pk, sk))
}

pub struct Wrapper {
    pubkey: PubKey,
    seckey: SecKey,
}
impl Wrapper {
    pub fn from_files(pk: &std::path::Path, sk: &std::path::Path) -> Result<Self> {
        Ok(Self {
            pubkey: PubKey::load(pk)?,
            seckey: SecKey::load(sk)?,
        })
    }
}
impl crate::wrap::Wrapper for Wrapper {
    fn wrap(&self, msg: &[u8]) -> Result<Vec<u8>> {
        sign(msg, &self.seckey)
    }
    fn unwrap(&self, msg: &[u8]) -> Result<Vec<u8>> {
        open(msg, &self.pubkey).ok_or(anyhow::Error::msg("unwrap failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_detached() -> Result<()> {
        let msg = vec![1, 2, 3, 4, 5];
        let (pk, sk) = keygen()?;
        let sig = sign_detached(&msg, &sk)?;
        println!("{sig:?}");
        assert!(verify_detached(&sig, &msg, &pk));
        Ok(())
    }
    #[test]
    fn test_sign_fail_detached() -> Result<()> {
        let msg = vec![1, 2, 3, 4, 5];
        let (pk, sk) = keygen()?;
        let mut sig = sign_detached(&msg, &sk)?;
        sig[3] ^= 8;
        println!("{sig:?}");
        assert!(!verify_detached(&sig, &msg, &pk));
        Ok(())
    }
    #[test]
    fn test_sign() -> Result<()> {
        let msg = vec![1, 2, 3, 4, 5];
        let (pk, sk) = keygen()?;
        let signed = sign(&msg, &sk)?;
        let opened = open(&signed, &pk).unwrap();
        assert_eq!(opened, msg);
        Ok(())
    }
    #[test]
    fn test_sign_fail() -> Result<()> {
        let msg = vec![1, 2, 3, 4, 5];
        let (pk, sk) = keygen()?;
        let mut signed = sign_detached(&msg, &sk)?;
        signed[3] ^= 8;
        assert_eq!(None, open(&signed, &pk));
        Ok(())
    }
}

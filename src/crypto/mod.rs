use anyhow::Result;
use log::error;
use std::io::Read;

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
            pubkey: vec![0; unsafe { agw_crypto_sign_PUBLICKEYBYTES } as usize],
        }
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
            seckey: vec![0; unsafe { agw_crypto_sign_SECRETKEYBYTES } as usize],
        }
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
}
#[link(name = "defines", kind = "static")]
extern "C" {
    static agw_crypto_sign_PUBLICKEYBYTES: libc::c_ulonglong;
    static agw_crypto_sign_SECRETKEYBYTES: libc::c_ulonglong;
    static agw_crypto_sign_BYTES: libc::c_ulonglong;
}

fn init() {
    unsafe {
        sodium_init();
    }
}

pub fn sign(msg: &[u8], key: &SecKey) -> Result<Vec<u8>> {
    init();
    let mut sig = vec![0u8; msg.len() + unsafe { agw_crypto_sign_BYTES } as usize];
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
    let mut sig = vec![0u8; unsafe { agw_crypto_sign_BYTES } as usize];
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
    assert_eq!(siglen, unsafe { agw_crypto_sign_BYTES });
    if rc == -1 {
        Err(anyhow::Error::msg("crypto_sign_detached() failed"))
    } else {
        Ok(sig[..(siglen as usize)].to_vec())
    }
}

pub fn open(sig: &[u8], pubkey: &PubKey) -> Option<Vec<u8>> {
    init();
    let siglen = sig.len();
    let rightlen = unsafe { agw_crypto_sign_BYTES } as usize;
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
    let rightlen = unsafe { agw_crypto_sign_BYTES } as usize;
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
    pub fn from_files(pk: &str, sk: &str) -> Result<Self> {
        let mut pkb = Vec::new();
        let mut skb = Vec::new();
        std::fs::File::open(pk)?.read_to_end(&mut pkb)?;
        std::fs::File::open(sk)?.read_to_end(&mut skb)?;
        if pkb.len() != unsafe { agw_crypto_sign_PUBLICKEYBYTES } as usize {
            return Err(anyhow::Error::msg("public key file has wrong size"));
        }
        if skb.len() != unsafe { agw_crypto_sign_SECRETKEYBYTES } as usize {
            return Err(anyhow::Error::msg("secret key has wrong size"));
        }
        Ok(Self {
            pubkey: PubKey { pubkey: pkb },
            seckey: SecKey { seckey: skb },
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

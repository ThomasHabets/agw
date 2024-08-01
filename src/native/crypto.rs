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
    fn crypto_sign_detached(
        sig: *mut libc::c_uchar,
        siglen: *mut libc::c_ulonglong,
        msg: *const libc::c_uchar,
        msglen: libc::c_ulonglong,
        sk: *const libc::c_uchar,
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

pub fn verify(sig: &[u8], msg: &[u8], pubkey: &PubKey) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign() -> Result<()> {
        let msg = vec![1, 2, 3, 4, 5];
        let (pk, sk) = keygen()?;
        let sig = sign(&msg, &sk)?;
        println!("{sig:?}");
        assert!(verify(&sig, &msg, &pk));
        Ok(())
    }
    #[test]
    fn test_sign_fail() -> Result<()> {
        let msg = vec![1, 2, 3, 4, 5];
        let (pk, sk) = keygen()?;
        let mut sig = sign(&msg, &sk)?;
        sig[3] ^= 8;
        println!("{sig:?}");
        assert!(!verify(&sig, &msg, &pk));
        Ok(())
    }
}

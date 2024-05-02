use anyhow::{Error, Result};

/** Callsign, including SSID.

Max length is 10, because that's the max length in the AGW
protocol.
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    bytes: [u8; 10],
}

impl Call {
    pub fn from_bytes(bytes: &[u8]) -> Result<Call> {
        if bytes.len() > 10 {
            return Err(Error::msg(format!(
                "callsign '{:?}' is longer than 10 characters",
                bytes
            )));
        }
        // NOTE: Callsigns here are not just real callsigns, but also
        // virtual ones like WIDE1-1 and APZ001.
        let mut arr = [0; 10];
        for (i, &item) in bytes.iter().enumerate() {
            // TODO: is slash valid?
            if item != 0 && !item.is_ascii_alphanumeric() && item != b'-' {
                return Err(Error::msg(format!(
                    "callsign includes invalid character {:?}",
                    item
                )));
            }
            arr[i] = item;
        }
        Ok(Call { bytes: arr })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn string(&self) -> String {
        let mut s = String::new();
        for ch in self.bytes.iter() {
            if *ch == 0 {
                break;
            }
            s.push(*ch as char);
        }
        s
    }

    /// Return true if the callsign is empty.
    ///
    /// Sometimes this is the correct thing, for incoming/outgoing AGW
    /// packets. E.g. querying the outgoing packet queue does not have
    /// source nor destination.
    pub fn is_empty(&self) -> bool {
        for b in self.bytes {
            if b != 0 {
                return false;
            }
        }
        true
    }
}

impl std::str::FromStr for Call {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_bytes(s.as_bytes())
    }
}

impl std::fmt::Display for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (n, ch) in self.bytes.iter().enumerate() {
            if *ch == 0 {
                let s = String::from_utf8(self.bytes[..n].to_vec()).expect("parsing string");
                return write!(f, "{s}");
            }
        }
        let s = String::from_utf8(self.bytes.to_vec()).expect("parsing string");
        write!(f, "{s}")
    }
}

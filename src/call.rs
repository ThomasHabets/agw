use crate::{Error, Result};

/// Callsign, including SSID.
///
/// Max length is 10, because that's the max length in the AGW protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    bytes: [u8; 10],
}

impl Call {
    /// Create callsign from ASCII bytes.
    ///
    /// # Errors
    ///
    /// If the callsign is in invalid format, such as containing non-ascii
    /// characters.
    pub fn from_bytes(bytes: &[u8]) -> Result<Call> {
        if bytes.len() > 10 {
            return Err(Error::Plain(format!(
                "callsign '{bytes:?}' is longer than 10 characters"
            )));
        }
        // NOTE: Callsigns here are not just real callsigns, but also
        // virtual ones like WIDE1-1 and APZ001.
        let mut arr = [0; 10];
        for (i, &item) in bytes.iter().enumerate() {
            // TODO: is slash valid?
            if item != 0 && !item.is_ascii_alphanumeric() && item != b'-' {
                return Err(Error::Plain(format!(
                    "callsign includes invalid character {item:?}"
                )));
            }
            arr[i] = item;
        }
        Ok(Call { bytes: arr })
    }

    /// Bytes of the callsign string.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Callsign as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        #[allow(clippy::missing_panics_doc)]
        str::from_utf8(&self.bytes).expect("can't happen: call contains non-UTF8")
    }

    /// Return true if the callsign is empty.
    ///
    /// Sometimes this is the correct thing, for incoming/outgoing AGW packets.
    /// E.g. querying the outgoing packet queue does not have source nor
    /// destination.
    #[must_use]
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
    type Err = Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
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

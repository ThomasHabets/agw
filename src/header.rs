use crate::{Call, Pid, Port};

#[derive(Clone, Debug)]
pub struct Header {
    pub port: Port,
    pub pid: Pid,
    pub data_kind: u8,
    pub data_len: u32,
    pub src: Option<Call>,
    pub dst: Option<Call>,
}
pub const HEADER_LEN: usize = 36;
impl Header {
    /// Create new header.
    // TODO remove this.
    #[must_use]
    pub fn new(
        port: Port,
        data_kind: u8,
        pid: Pid,
        src: Option<Call>,
        dst: Option<Call>,
        data_len: u32,
    ) -> Header {
        Header {
            port,
            pid,
            data_kind,
            data_len,
            src,
            dst,
        }
    }

    /// Serialize header.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut v = vec![0; HEADER_LEN];
        // Either the port parameter is not used (such as version or port info
        // query), or it maps port 1 to byte 0, 2 to 1, etc.
        v[0] = self.port.0.saturating_sub(1);
        v[4] = self.data_kind;
        v[6] = self.pid.0;

        if let Some(src) = &self.src {
            v.splice(8..18, src.as_bytes().iter().copied());
        }
        if let Some(dst) = &self.dst {
            v.splice(18..28, dst.as_bytes().iter().copied());
        }
        v.splice(28..32, u32::to_le_bytes(self.data_len));
        v
    }
}

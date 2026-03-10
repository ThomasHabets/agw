use crate::{Call, Pid, Port};

#[derive(Clone, Debug)]
pub struct Header {
    port: Port,
    pid: Pid,
    data_kind: u8,
    data_len: u32,
    src: Option<Call>,
    dst: Option<Call>,
}
pub const HEADER_LEN: usize = 36;
impl Header {
    pub fn port(&self) -> Port {
        self.port
    }
    pub fn pid(&self) -> Pid {
        self.pid
    }
    pub fn data_kind(&self) -> u8 {
        self.data_kind
    }
    pub fn data_len(&self) -> u32 {
        self.data_len
    }
    pub fn src(&self) -> &Option<Call> {
        &self.src
    }
    pub fn dst(&self) -> &Option<Call> {
        &self.dst
    }

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
            data_kind,
            pid,
            data_len,
            src,
            dst,
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut v = vec![0; HEADER_LEN];
        v[0] = self.port.0;
        v[4] = self.data_kind;
        v[6] = self.pid.0;

        if let Some(src) = &self.src {
            v.splice(8..18, src.as_bytes().iter().cloned());
        }
        if let Some(dst) = &self.dst {
            v.splice(18..28, dst.as_bytes().iter().cloned());
        }
        v.splice(28..32, u32::to_le_bytes(self.data_len));
        v
    }
}

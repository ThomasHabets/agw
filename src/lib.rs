use anyhow::{Error, Result};
use log::{debug, warn};
use std::collections::LinkedList;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;

fn port_info() -> Vec<u8> {
    Header::new(0, b'G', 0, None, None, 0)
        .serialize()
        .expect("can't happen: port_info serialization failed")
}

fn port_cap(port: u8) -> Vec<u8> {
    Header::new(port, b'g', 0, None, None, 0)
        .serialize()
        .expect("can't happen: port_cap serialization failed")
}

fn version_info() -> Vec<u8> {
    Header::new(0, b'R', 0, None, None, 0)
        .serialize()
        .expect("can't happen: version_info serialization failed")
}

fn connect(port: u8, pid: u8, src: &Call, dst: &Call) -> Result<Vec<u8>> {
    Header::new(port, b'C', pid, Some(src.clone()), Some(dst.clone()), 0).serialize()
}

fn connect_via(port: u8, pid: u8, src: &Call, dst: &Call, via: &[Call]) -> Result<Vec<u8>> {
    let h = Header::new(port, b'v', pid, Some(src.clone()), Some(dst.clone()), 0).serialize()?;
    const MAX_HOPS: usize = 7;
    if via.len() > MAX_HOPS {
        return Err(Error::msg(format!(
            "tried to connect through too many hops: {} > {MAX_HOPS}",
            via.len()
        )));
    }

    let mut hops = Vec::new();
    hops.push(via.len() as u8);
    for call in via {
        hops.extend_from_slice(&call.bytes);
    }
    Ok([h, hops.to_vec()].concat())
}

fn disconnect(port: u8, pid: u8, src: &Call, dst: &Call) -> Result<Vec<u8>> {
    Header::new(port, b'd', pid, Some(src.clone()), Some(dst.clone()), 0).serialize()
}

fn make_unproto(port: u8, pid: u8, src: &Call, dst: &Call, data: &[u8]) -> Result<Vec<u8>> {
    let h = Header::new(
        port,
        b'M',
        pid,
        Some(src.clone()),
        Some(dst.clone()),
        data.len() as u32,
    )
    .serialize()?;
    Ok([h, data.to_vec()].concat())
}

#[derive(Clone, Debug)]
pub struct Call {
    bytes: [u8; 10],
}

impl Call {
    fn parse(bytes: &[u8]) -> Call {
        let mut arr = [0; 10];
        for (i, &item) in bytes.iter().enumerate() {
            arr[i] = item;
        }
        Call { bytes: arr }
    }
    pub fn from_str(s: &str) -> Result<Call> {
        if s.len() > 10 {
            return Err(Error::msg(format!(
                "callsign '{}' is longer than 10 characters",
                s
            )));
        }
        let mut arr = [0; 10];
        for (i, &item) in s.as_bytes().iter().enumerate() {
            arr[i] = item;
        }
        Ok(Call { bytes: arr })
    }

    pub fn is_empty(&self) -> bool {
        for b in self.bytes {
            if b != 0 {
                return false;
            }
        }
        true
    }
}

enum Reply {
    // TODO: should these actually pick up the header value subset,
    // too, when appropriate?
    Version(u16, u16),                // R.
    CallsignRegistration(bool),       // X.
    PortInfo(String),                 // G. TODO: parse
    PortCaps(String),                 // g. TODO: parse
    FramesOutstandingPort(u32),       // y.
    FramesOutstandingConnection(u32), // Y.
    HeardStations(String),            // H. TODO: parse
    Connected(String),                // C.
    ConnectedData(Vec<u8>),           // D.
    MonitorConnected(Vec<u8>),        // I.
    MonitorSupervisory(Vec<u8>),      // S.
    Unproto(Vec<u8>),                 // U.
    ConnectedSent(Vec<u8>),           // T.
    Raw(Vec<u8>),                     // R.
    Unknown(Header, Vec<u8>),
}

impl Reply {
    fn description(&self) -> String {
        match self {
            Reply::ConnectedData(data) => format!("ConnectedData: {:?}", data),
            Reply::ConnectedSent(data) => format!("ConnectedSent: {:?}", data),
            Reply::Unproto(data) => format!("Received unproto: {:?}", data),
            Reply::PortInfo(s) => format!("Port info: {}", s),
            Reply::PortCaps(s) => format!("Port caps: {}", s),
            Reply::Connected(s) => format!("Connected: {}", s),
            Reply::Version(maj, min) => format!("Version: {maj}.{min}"),
            Reply::Raw(_data) => "Raw".to_string(),
            Reply::CallsignRegistration(success) => format!("Callsign registration: {success}"),
            Reply::FramesOutstandingPort(n) => format!("Frames outstanding port: {n}"),
            Reply::FramesOutstandingConnection(n) => format!("Frames outstanding connection: {n}"),
            Reply::MonitorConnected(_) => format!("Connected packet"),
            Reply::MonitorSupervisory(_) => format!("Supervisory packet"),
            Reply::HeardStations(s) => format!("Heard stations: {s}"),
            Reply::Unknown(h, data) => format!("Unknown reply: header={h:?} data={data:?}"),
        }
    }
}

fn parse_reply(header: &Header, data: &[u8]) -> Result<Reply> {
    // TODO: confirm data len, since most replies will have fixed size.
    Ok(match header.data_kind {
        b'R' => {
            let major = u16::from_le_bytes(
                data[0..2]
                    .try_into()
                    .expect("can't happen: two bytes can't be made into u16?"),
            );
            let minor = u16::from_le_bytes(
                data[4..6]
                    .try_into()
                    .expect("can't happen: two bytes can't be made into u16?"),
            );
            Reply::Version(major, minor)
        }
        b'X' => Reply::CallsignRegistration(data[0] == 1),
        b'C' => Reply::Connected(std::str::from_utf8(data)?.to_string()),
        b'D' => Reply::ConnectedData(data.to_vec()),
        b'T' => Reply::ConnectedSent(data.to_vec()),
        b'U' => Reply::Unproto(data.to_vec()),
        b'G' => Reply::PortInfo(std::str::from_utf8(data)?.to_string()),
        b'g' => {
            let rate = data[0];
            let traffic_level = data[1];
            let tx_delay = data[2];
            let tx_tail = data[3];
            let persist = data[4];
            let slot_time = data[5];
            let max_frame = data[6];
            let active_connections = data[7];
            let bytes_per_2min = u32::from_le_bytes(data[8..12].try_into().unwrap());

            Reply::PortCaps(format![
                "rate={rate}
  traffic={traffic_level}
  txdelay={tx_delay}
  txtail={tx_tail}
  persist={persist}
  slot_time={slot_time}
  max_frame={max_frame}
  active_connections={active_connections}
  bytes_per_2min={bytes_per_2min}"
            ])
        }
        b'y' => Reply::FramesOutstandingPort(u32::from_le_bytes(data[0..4].try_into().unwrap())),
        b'Y' => {
            Reply::FramesOutstandingConnection(u32::from_le_bytes(data[0..4].try_into().unwrap()))
        }
        b'H' => Reply::HeardStations(std::str::from_utf8(data)?.to_string()),
        b'I' => Reply::MonitorConnected(data.to_vec()),
        b'S' => Reply::MonitorSupervisory(data.to_vec()),
        b'K' => Reply::Raw(data.to_vec()),
        _ => Reply::Unknown(header.clone(), data.to_vec()),
    })
}

pub struct Connection<'a> {
    port: u8,
    pid: u8,
    src: Call,
    dst: Call,
    agw: &'a mut AGW,
    disconnected: bool,
}

impl<'a> Connection<'a> {
    fn new(agw: &'a mut AGW, port: u8, pid: u8, src: Call, dst: Call) -> Connection {
        Connection {
            port,
            pid,
            src,
            dst,
            agw,
            disconnected: false,
        }
    }
    pub fn disconnect(&mut self) -> Result<()> {
        if !self.disconnected {
            eprintln!("disc");
            debug!("disconnecting");
            self.agw
                .send(&disconnect(self.port, self.pid, &self.src, &self.dst)?)?;
            self.disconnected = true;
        }
        Ok(())
    }
}

impl<'a> Drop for Connection<'a> {
    fn drop(&mut self) {
        if let Err(e) = self.disconnect() {
            warn!("drop-disconnection errored with {:?}", e);
        }
    }
}

#[derive(Clone, Debug)]
struct Header {
    port: u8,
    pid: u8,
    data_kind: u8,
    data_len: u32,
    src: Option<Call>,
    dst: Option<Call>,
}
const HEADER_LEN: usize = 36;
impl Header {
    fn new(
        port: u8,
        data_kind: u8,
        pid: u8,
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

    fn serialize(&self) -> Result<Vec<u8>> {
        let mut v = vec![0; HEADER_LEN];
        v[0] = self.port;
        v[4] = self.data_kind;
        v[6] = self.pid;

        if let Some(src) = &self.src {
            v.splice(8..18, src.bytes.iter().cloned());
        }
        if let Some(dst) = &self.dst {
            v.splice(18..28, dst.bytes.iter().cloned());
        }
        v.splice(28..32, u32::to_le_bytes(self.data_len));
        Ok(v)
    }
}

fn parse_header(header: &[u8; HEADER_LEN]) -> Header {
    let src = Call::parse(&header[8..18]);
    let src = if src.is_empty() { None } else { Some(src) };
    let dst = Call::parse(&header[18..28]);
    let dst = if dst.is_empty() { None } else { Some(dst) };
    Header {
        port: header[0],
        data_kind: header[4],
        pid: header[6],
        src: src,
        dst: dst,
        data_len: u32::from_le_bytes(header[28..32].try_into().unwrap()),
    }
}

pub struct AGW {
    rx: mpsc::Receiver<(Header, Reply)>,
    rxqueue: LinkedList<(Header, Reply)>,
    stream: TcpStream,
}

impl AGW {
    pub fn new(addr: &str) -> Result<AGW> {
        let (tx, rx) = mpsc::channel();
        let stream = TcpStream::connect(addr)?;
        let agw = AGW {
            rx,
            stream: stream.try_clone()?,
            rxqueue: LinkedList::new(),
        };
        std::thread::spawn(|| {
            if let Err(e) = Self::reader(stream, tx) {
                warn!("TCP socket reader connected to AGWPE ended: {:?}", e);
            }
        });
        Ok(agw)
    }

    fn send(&mut self, msg: &[u8]) -> Result<()> {
        self.stream.write(&msg)?;
        Ok(())
    }

    fn reader(mut stream: TcpStream, tx: mpsc::Sender<(Header, Reply)>) -> Result<()> {
        loop {
            let mut header = [0 as u8; HEADER_LEN];
            stream.read_exact(&mut header)?;
            let header = parse_header(&header);
            let payload = if header.data_len > 0 {
                let mut payload = vec![0; header.data_len as usize];
                stream.read_exact(&mut payload)?;
                payload
            } else {
                Vec::new()
            };
            let reply = parse_reply(&header, &payload)?;
            debug!("Got reply: {}", reply.description());
            tx.send((header, reply))?;
        }
    }

    fn rx_enqueue(&mut self, h: Header, r: Reply) {
        self.rxqueue.push_back((h, r));
        const WARN_LIMIT: usize = 10;
        let l = self.rxqueue.len();
        if l > WARN_LIMIT {
            warn!("AGW maxqueue length {l} > {WARN_LIMIT}");
        }
    }

    pub fn version(&mut self) -> Result<(u16, u16)> {
        self.send(&version_info())?;
        loop {
            let (h, r) = self.rx.recv()?;
            match r {
                Reply::Version(maj, min) => return Ok((maj, min)),
                other => self.rx_enqueue(h, other),
            }
        }
    }
    pub fn port_info(&mut self) -> Result<String> {
        self.send(&port_info())?;
        loop {
            let (h, r) = self.rx.recv()?;
            match r {
                Reply::PortInfo(i) => return Ok(i),
                other => self.rx_enqueue(h, other),
            }
        }
    }
    pub fn port_cap(&mut self, port: u8) -> Result<String> {
        self.send(&port_cap(port))?;
        loop {
            let (h, r) = self.rx.recv()?;
            match r {
                Reply::PortCaps(i) => return Ok(i),
                other => self.rx_enqueue(h, other),
            }
        }
    }

    pub fn unproto(
        &mut self,
        port: u8,
        pid: u8,
        src: &Call,
        dst: &Call,
        data: &[u8],
    ) -> Result<()> {
        self.send(&make_unproto(port, pid, src, dst, data)?)?;
        Ok(())
    }

    pub fn connect<'a>(
        &'a mut self,
        port: u8,
        pid: u8,
        src: &Call,
        dst: &Call,
        via: &[Call],
    ) -> Result<Connection<'a>> {
        if via.len() == 0 {
            self.send(&connect(port, pid, src, dst)?)?;
        } else {
            self.send(&connect_via(port, pid, src, dst, via)?)?;
            todo!();
        }
        /*loop {
            let (h, r) = self.rx.recv()?;
            match r {
                Reply::Connected(i) => return Ok(i),
                other => self.rx_enqueue(h, other),
            }
        }*/
        Ok(Connection::new(self, port, pid, src.clone(), dst.clone()))
    }
}

use anyhow::{Error, Result};
use log::{debug, warn};
use std::collections::LinkedList;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;

fn port_info(port: u8) -> Vec<u8> {
    Header::new(port, b'G', 0, None, None, 0)
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

fn write_connected(port: u8, pid: u8, src: &Call, dst: &Call, data: &[u8]) -> Result<Vec<u8>> {
    let h = Header::new(
        port,
        b'D',
        pid,
        Some(src.clone()),
        Some(dst.clone()),
        data.len() as u32,
    )
    .serialize()?;
    Ok([h, data.to_vec()].concat())
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

fn register_callsign(port: u8, pid: u8, src: &Call) -> Result<Vec<u8>> {
    Header::new(port, b'X', pid, Some(src.clone()), None, 0).serialize()
}

// TODO: unregister with 'x'

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

/** Callsign, including SSID.

Max length is 10, because that's the max length in the AGW
protocol.
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    bytes: [u8; 10],
}

impl Call {
    fn from_bytes(bytes: &[u8]) -> Result<Call> {
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

    /// Create Call from string. Include SSID.
    ///
    /// Max length is 10, because that's the max length in the AGW
    /// protocol.
    pub fn from_str(s: &str) -> Result<Call> {
        Self::from_bytes(&s.as_bytes())
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

impl std::fmt::Display for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (n, ch) in self.bytes.iter().enumerate() {
            if *ch == 0 {
                let s = String::from_utf8(self.bytes[..n].to_vec()).unwrap();
                return write!(f, "{s}");
            }
        }
        let s = String::from_utf8(self.bytes.to_vec()).unwrap();
        write!(f, "{s}")
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
    Disconnect,                       // d.
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
            Reply::Disconnect => format!("Disconnect"),
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
        b'd' => Reply::Disconnect,
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

/// AX.25 connection object.
///
/// Created from an AGW object, using `.connect()`.
pub struct Connection<'a> {
    port: u8,
    pid: u8,
    src: Call,
    dst: Call,
    agw: &'a mut AGW,
    disconnected: bool,
}

/// An object that has all the metadata needed to be able to create
/// AGW "write some stuff on the established connection", without
/// owning the whole connection object.
///
/// See examples/term.rs for example use.
pub struct MakeWriter {
    port: u8,
    pid: u8,
    src: Call,
    dst: Call,
}
impl MakeWriter {
    /// Make the bytes of an AGW packet to send a packet of data.
    pub fn make(&self, data: &[u8]) -> Vec<u8> {
        write_connected(self.port, self.pid, &self.src, &self.dst, data).unwrap()
    }
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

    /// Read user data from the connection.
    pub fn read(&mut self) -> Result<Vec<u8>> {
        self.agw.read_connected(&self.src, &self.dst)
    }

    /// Write data to the connection.
    pub fn write(&mut self, data: &[u8]) -> Result<usize> {
        self.agw
            .write_connected(self.port, self.pid, &self.src, &self.dst, data)
    }

    /// Create MakeWriter object, in order to create AGW packets
    /// without holding on to a connection.
    pub fn make_writer(&self) -> MakeWriter {
        MakeWriter {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
        }
    }

    /// Return a copy of the mpsc to send bytes on the AGW connection.
    ///
    /// TODO: this should probably be abstracted away.
    pub fn sender(&mut self) -> mpsc::Sender<Vec<u8>> {
        self.agw.sender()
    }

    /// Disconnect the connection.
    pub fn disconnect(&mut self) -> Result<()> {
        if !self.disconnected {
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

fn parse_header(header: &[u8; HEADER_LEN]) -> Result<Header> {
    let src = Call::from_bytes(&header[8..18])?;
    let src = if src.is_empty() { None } else { Some(src) };
    let dst = Call::from_bytes(&header[18..28])?;
    let dst = if dst.is_empty() { None } else { Some(dst) };
    Ok(Header {
        port: header[0],
        data_kind: header[4],
        pid: header[6],
        src: src,
        dst: dst,
        data_len: u32::from_le_bytes(header[28..32].try_into().unwrap()),
    })
}

/// AGW connection.
pub struct AGW {
    rx: mpsc::Receiver<(Header, Reply)>,

    // Write entire frames.
    tx: mpsc::Sender<Vec<u8>>,

    // TODO: LinkedList is not awesome, because it's O(n) to remove an
    // element in the middle.
    // Maybe once Rust RFC2570 gets solved, it'll all be fine.
    rxqueue: LinkedList<(Header, Reply)>,
}

impl AGW {
    /// Create AGW connection to ip:port.
    pub fn new(addr: &str) -> Result<AGW> {
        let (tx, rx) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let wstream = TcpStream::connect(addr)?;
        let rstream = wstream.try_clone()?;
        let agw = AGW {
            rx,
            tx: tx2,
            rxqueue: LinkedList::new(),
        };
        // Start reader.
        std::thread::spawn(|| {
            if let Err(e) = Self::reader(rstream, tx) {
                warn!("TCP socket reader connected to AGWPE ended: {:?}", e);
            }
        });
        // Start writer.
        std::thread::spawn(|| {
            if let Err(e) = Self::writer(wstream, rx2) {
                warn!("TCP socket writer connected to AGWPE ended: {:?}", e);
            }
        });
        Ok(agw)
    }

    fn send(&mut self, msg: &[u8]) -> Result<()> {
        self.tx.send(msg.to_vec())?;
        Ok(())
    }

    fn sender(&mut self) -> mpsc::Sender<Vec<u8>> {
        self.tx.clone()
    }

    fn writer(mut stream: TcpStream, rx: mpsc::Receiver<Vec<u8>>) -> Result<()> {
        loop {
            let buf = rx.recv()?;
            stream.write(&buf)?;
        }
    }

    fn reader(mut stream: TcpStream, tx: mpsc::Sender<(Header, Reply)>) -> Result<()> {
        loop {
            let mut header = [0 as u8; HEADER_LEN];
            stream.read_exact(&mut header)?;
            let header = parse_header(&header)?;
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

    /// Get the version of the AGW endpoint.
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

    /// Get some port info for the AGW endpoint.
    pub fn port_info(&mut self, port: u8) -> Result<String> {
        self.send(&port_info(port))?;
        loop {
            let (h, r) = self.rx.recv()?;
            match r {
                Reply::PortInfo(i) => return Ok(i),
                other => self.rx_enqueue(h, other),
            }
        }
    }

    /// Get port capabilities of the AGW "port".
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

    /// Send UI packet.
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

    /// Register callsign.
    ///
    /// The specs say that registering the callsign is
    /// mandatory. Direwolf doesn't seem to care, but there it is.
    ///
    /// Presumably needed for incoming connection, but incoming
    /// connections are not tested yet.
    pub fn register_callsign(&mut self, port: u8, pid: u8, src: &Call) -> Result<()> {
        debug!("Registering callsign");
        self.send(&register_callsign(port, pid, src)?)?;
        Ok(())
    }

    /// Create a new connection.
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
        loop {
            let (head, r) = self.rx.recv()?;
            if head.src.as_ref().map_or(true, |x| x != dst)
                || head.dst.as_ref().map_or(true, |x| x != src)
            {
                //eprintln!("Got packet not for us");
                continue;
            }
            match r {
                Reply::Connected(i) => {
                    debug!("Connected from {src} to {dst} with connect string {i}");
                    break;
                }
                other => self.rx_enqueue(head, other),
            }
        }
        Ok(Connection::new(self, port, pid, src.clone(), dst.clone()))
    }

    fn write_connected(
        &mut self,
        port: u8,
        pid: u8,
        src: &Call,
        dst: &Call,
        data: &[u8],
    ) -> Result<usize> {
        // TODO: enforce max size?
        let len = data.len();
        if len > 0 {
            self.send(&write_connected(port, pid, src, dst, data)?)?;
        }
        Ok(data.len())
    }

    fn read_connected(&mut self, me: &Call, remote: &Call) -> Result<Vec<u8>> {
        // First check the existing queue.
        for frame in self.rxqueue.iter().enumerate() {
            let (n, (head, payload)) = &frame;
            if head.src.as_ref().map_or(true, |x| x != remote)
                || head.dst.as_ref().map_or(true, |x| x != me)
            {
                continue;
            }
            match payload {
                Reply::ConnectedData(data) => {
                    let ret = data.to_vec();
                    let mut tail = self.rxqueue.split_off(*n);
                    tail.pop_front();
                    self.rxqueue.append(&mut tail);
                    return Ok(ret);
                }
                Reply::Disconnect => {
                    return Err(Error::msg("remote end disconnected"));
                }
                _ => {
                    debug!("Remote end send unexpected data {}", payload.description());
                }
            }
        }

        // Next packet not in the queue. Wait.
        loop {
            let (h, r) = self.rx.recv()?;
            match r {
                Reply::ConnectedData(i) => return Ok(i),
                other => self.rx_enqueue(h, other),
            }
        }
    }
}

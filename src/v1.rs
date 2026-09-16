use log::{debug, trace, warn};
use std::collections::LinkedList;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::HEADER_LEN;
use crate::{Call, Header, Packet, Pid, Port};
use crate::{Error, Result};

const CALLSIGN_HEARD_REPLIES: usize = 20;
const CALLSIGN_HEARD_TIMEOUT: Duration = Duration::from_secs(1);

// TODO: get rid of Reply struct. It's just a subset of Packet.

/// Info about one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    pub port: Port,
    pub descr: String,
}

/// Info about all ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortsInfo {
    /// Number of ports.
    pub count: usize,

    /// Description of ports.
    pub ports: Vec<PortInfo>,
}

/// Baud rate.
///
/// Normally 1200 or 9600 for classic AX.25.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baud {
    Unknown,
    B1200,
    B2400,
    B4800,
    B9600,
}

impl Baud {
    fn from_byte(b: u8) -> Option<Baud> {
        Some(match b {
            0 => Baud::B1200,
            1 => Baud::B2400,
            2 => Baud::B4800,
            3 => Baud::B9600,
            _ => return None,
        })
    }
}

/// Port capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCaps {
    /// On air baud rate.
    pub rate: Baud,

    /// Traffic level.
    ///
    /// `None` if port is not in autoupdate mode.
    pub traffic_level: Option<u8>,

    // TODO: get units on these.
    pub tx_tail: u8,
    pub tx_delay: u8,
    pub persist: u8,
    pub slot_time: u8,
    pub max_frame: u8,

    /// How many connections are active on this port
    pub active_connections: u8,

    /// How many bytes received in the last 2 minutes as a 32 bits (4 bytes)
    /// integer. Updated every two minutes.
    pub bytes_per_2min: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallsignHeard {
    pub call: Call,
    // TODO: timestamps.
}

#[derive(Debug, Clone)]
pub(crate) struct ConnectedData {
    pub port: Port,
    pub pid: Pid,
    pub src: Call,
    pub dst: Call,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct Connected {
    pub port: Port,
    pub pid: Pid,
    pub src: Call,
    pub dst: Call,
    pub data: String,
}

#[derive(Debug, Clone)]
pub(crate) enum Reply {
    // TODO: should these actually pick up the header value subset,
    // too, when appropriate?
    Version(u16, u16),                       // R.
    CallsignRegistration(bool),              // X.
    PortInfo(PortsInfo),                     // G.
    PortCaps(Port, PortCaps),                // g.
    FramesOutstandingPort(Port, usize),      // y.
    FramesOutstandingConnection(u32),        // Y.
    CallsignHeard(Port, Vec<CallsignHeard>), // H.
    ConnectionEstablished(Connected),        // C.
    ConnectionFailed(Connected),             // C.
    IncomingConnection(Connected),           // C.
    ConnectedData(ConnectedData),            // D.
    Disconnect,                              // d.
    MonitorConnected(Vec<u8>),               // I.
    MonitorSupervisory(Vec<u8>),             // S.
    Unproto(Vec<u8>),                        // U.
    ConnectedSent(Vec<u8>),                  // T.
    Raw(Vec<u8>),                            // R.
    Unknown(Header, Vec<u8>),
}

impl Reply {
    fn description(&self) -> String {
        match self {
            Reply::Disconnect => "Disconnect".to_string(),
            Reply::ConnectedData(data) => format!("ConnectedData: {data:?}"),
            Reply::ConnectedSent(data) => format!("ConnectedSent: {data:?}"),
            Reply::Unproto(data) => format!("Received unproto: {data:?}"),
            Reply::PortInfo(s) => format!("Port info: {s:?}"),
            Reply::PortCaps(port, s) => format!("Port caps for port {port:?}: {s:?}"),
            Reply::ConnectionEstablished(s) => format!("Connected: {s:?}"),
            Reply::ConnectionFailed(s) => format!("Connection failed: {s:?}"),
            Reply::IncomingConnection(s) => format!("Incoming connection: {s:?}"),
            Reply::Version(maj, min) => format!("Version: {maj}.{min}"),
            Reply::CallsignHeard(port, c) => format!("Heard on {port:?}: {c:?}"),
            Reply::Raw(_data) => "Raw".to_string(),
            Reply::CallsignRegistration(success) => format!("Callsign registration: {success}"),
            Reply::FramesOutstandingPort(port, n) => {
                format!("Frames outstanding port {port:?}: {n}")
            }
            Reply::FramesOutstandingConnection(n) => format!("Frames outstanding connection: {n}"),
            Reply::MonitorConnected(x) => format!("Connected packet len {}", x.len()),
            Reply::MonitorSupervisory(x) => format!("Supervisory packet len {}", x.len()),
            Reply::Unknown(h, data) => format!("Unknown reply: header={h:?} data={data:?}"),
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_reply(header: &Header, data: &[u8]) -> Result<Reply> {
    // TODO: confirm data len, since most replies will have fixed size.
    Ok(match header.data_kind {
        b'R' => {
            if data.len() != 8 {
                return Err(Error::msg(format!(
                    "bad Version packet length {}, want 8",
                    data.len()
                )));
            }
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
        b'X' => {
            if data.len() != 1 {
                return Err(Error::msg(format!(
                    "bad CallsignRegistration length {}, want 1",
                    data.len(),
                )));
            }
            Reply::CallsignRegistration(data[0] == 1)
        }
        b'C' => {
            // AGW connection status messages may be terminated by CR, LF,
            // and/or NUL. They are framing artifacts, not status text.
            let data = std::str::from_utf8(data)
                .map_err(Error::other)?
                .trim_end_matches(['\0', '\r', '\n'])
                .to_string();
            let connection = Connected {
                port: header.port,
                pid: header.pid,
                src: header
                    .src
                    .clone()
                    .ok_or(Error::msg("connection established missing src"))?,
                dst: header
                    .dst
                    .clone()
                    .ok_or(Error::msg("connection established missing dst"))?,
                data,
            };
            if connection.data.starts_with("*** CONNECTED To Station") {
                Reply::IncomingConnection(connection)
            } else if connection.data.starts_with("*** CONNECTED") {
                Reply::ConnectionEstablished(connection)
            } else {
                // Is a `C` with a nonstandard message really the way connection
                // failed is communicated? Seems to me that it should be a `d`
                // frame, no?
                Reply::ConnectionFailed(connection)
            }
        }
        b'D' => Reply::ConnectedData(ConnectedData {
            port: header.port,
            pid: header.pid,
            src: header
                .src
                .clone()
                .ok_or(Error::msg("connected data missing src"))?,
            dst: header
                .dst
                .clone()
                .ok_or(Error::msg("connected data missing dst"))?,
            data: data.to_vec(),
        }),
        b'd' => Reply::Disconnect,
        b'T' => Reply::ConnectedSent(data.to_vec()),
        b'U' => Reply::Unproto(data.to_vec()),
        b'G' => {
            let re = regex::Regex::new(r"^Port(\d+)\s*(.*)$").unwrap();

            let s = std::str::from_utf8(data).map_err(Error::other)?;
            let (count, ports) = {
                let mut np = s.splitn(2, ';');
                let count = np
                    .next()
                    .ok_or(Error::msg("port info reply missing count"))?
                    .parse()
                    .map_err(Error::other)?;
                let ports = np
                    .next()
                    .ok_or(Error::msg("port info reply missing ports"))?
                    .split(';')
                    .map(std::string::ToString::to_string)
                    .filter(|s| s != "\0")
                    .map(|s| {
                        let caps = re
                            .captures(&s)
                            .ok_or(Error::msg(format!("bad port line {s:?}")))?;
                        let port = Port(
                            caps.get(1)
                                .ok_or(Error::msg("Can't happen: port number missing"))?
                                .as_str()
                                .parse()
                                .ok()
                                .ok_or(Error::msg("Port number not a number"))?,
                        );
                        let descr = caps
                            .get(2)
                            .ok_or(Error::msg("Can't happen: descr string missing"))?
                            .as_str()
                            .to_string();
                        Ok::<_, Error>(PortInfo { port, descr })
                    })
                    .collect::<Result<Vec<_>>>()?;
                (count, ports)
            };
            Reply::PortInfo(PortsInfo { count, ports })
        }
        b'g' => {
            if data.len() != 12 {
                return Err(Error::msg(format!(
                    "bad PortCaps length {}, want 12",
                    data.len()
                )));
            }
            let rate = data[0];
            let traffic_level = data[1];
            let tx_delay = data[2];
            let tx_tail = data[3];
            let persist = data[4];
            let slot_time = data[5];
            let max_frame = data[6];
            let active_connections = data[7];
            let bytes_per_2min =
                u32::from_le_bytes(data[8..12].try_into().expect("can't happen: bytes to u32"));

            let traffic_level = if traffic_level == 0xff {
                None
            } else {
                Some(traffic_level)
            };

            Reply::PortCaps(
                header.port,
                PortCaps {
                    rate: Baud::from_byte(rate).unwrap_or(Baud::Unknown),
                    traffic_level,
                    tx_delay,
                    tx_tail,
                    slot_time,
                    max_frame,
                    active_connections,
                    bytes_per_2min,
                    persist,
                },
            )
        }
        b'y' => {
            if data.len() != 4 {
                return Err(Error::msg(format!(
                    "bad FramesOutstdanding length {}, want 4",
                    data.len()
                )));
            }
            Reply::FramesOutstandingPort(
                header.port,
                usize::try_from(u32::from_le_bytes(
                    data[0..4].try_into().expect("can't happen: bytes to u32"),
                ))
                .expect("TODO: some error"),
            )
        }
        b'Y' => {
            if data.len() != 4 {
                return Err(Error::msg(format!(
                    "bad FramesOutstdandingConnection length {}, want 4",
                    data.len()
                )));
            }
            Reply::FramesOutstandingConnection(u32::from_le_bytes(
                data[0..4].try_into().expect("can't happen: bytes to u32"),
            ))
        }
        b'H' => Reply::CallsignHeard(header.port, parse_callsign_heard(data)?),
        b'I' => Reply::MonitorConnected(data.to_vec()),
        b'S' => Reply::MonitorSupervisory(data.to_vec()),
        b'K' => Reply::Raw(data.to_vec()),
        _ => Reply::Unknown(header.clone(), data.to_vec()),
    })
}

pub(crate) fn parse_callsign_heard(data: &[u8]) -> Result<Vec<CallsignHeard>> {
    let text_len = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let text = std::str::from_utf8(&data[..text_len]).map_err(Error::other)?;
    let Some(call) = text.split_ascii_whitespace().next() else {
        return Ok(vec![]);
    };

    // Empty H entries contain timestamp text but no callsign.
    let Ok(call) = Call::from_bytes(call.as_bytes()) else {
        return Ok(vec![]);
    };
    Ok(vec![CallsignHeard { call }])
}

/// An object that has all the metadata needed to be able to create
/// AGW "write some stuff on the established connection", without
/// owning the whole connection object.
///
/// See examples/term.rs for example use.
#[derive(Clone)]
pub struct MakeWriter {
    port: Port,
    pid: Pid,
    src: Call,
    dst: Call,
}
impl MakeWriter {
    /// Make the bytes of an AGW packet to send a packet of data.
    ///
    /// # Errors
    ///
    /// If given data so bad that the serialization fails.
    pub fn data<T: Into<Vec<u8>>>(&self, data: T) -> Result<Vec<u8>> {
        Packet::Data {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
            data: data.into(),
        }
        .serialize()
    }
    /// Make a disconnect packet.
    pub fn disconnect(&self) -> Result<Vec<u8>> {
        Packet::Disconnect {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
        }
        .serialize()
    }
}

/// AX.25 connection object.
///
/// Created from an AGW object, using `.connect()`.
pub struct Connection<'a> {
    port: Port,
    connect_string: String,
    pid: Pid,
    src: Call,
    dst: Call,
    agw: &'a mut AGW,
    disconnected: bool,
}

impl<'a> Connection<'a> {
    fn new(
        agw: &'a mut AGW,
        port: Port,
        connect_string: String,
        pid: Pid,
        src: Call,
        dst: Call,
    ) -> Self {
        Connection {
            port,
            connect_string,
            pid,
            src,
            dst,
            agw,
            disconnected: false,
        }
    }

    /// Return the connect string.
    #[must_use]
    pub fn connect_string(&self) -> &str {
        &self.connect_string
    }

    /// Read user data from the connection.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn read(&mut self) -> Result<Vec<u8>> {
        self.agw
            .read_connected(self.port, self.pid, &self.src, &self.dst)
    }

    /// Write data to the connection.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn write(&mut self, data: &[u8]) -> Result<usize> {
        self.agw
            .write_connected(self.port, self.pid, &self.src, &self.dst, data)
    }

    /// Create MakeWriter object, in order to create AGW packets
    /// without holding on to a connection.
    #[must_use]
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
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn disconnect(&mut self) -> Result<()> {
        if !self.disconnected {
            debug!("agw: disconnecting");
            let packet = Packet::Disconnect {
                port: self.port,
                pid: self.pid,
                src: self.src.clone(),
                dst: self.dst.clone(),
            };
            self.agw.send_packet(&packet)?;
            self.disconnected = true;
        }
        Ok(())
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.disconnect() {
            warn!("drop-disconnection errored with {e:?}");
        }
    }
}

/// Parse header from bytes.
///
/// # Errors
///
/// If the header is invalid.
#[allow(clippy::missing_panics_doc)]
pub fn parse_header(header: &[u8; HEADER_LEN]) -> Result<Header> {
    let src = Call::from_bytes(&header[8..18])?;
    let src = if src.is_empty() { None } else { Some(src) };
    let dst = Call::from_bytes(&header[18..28])?;
    let dst = if dst.is_empty() { None } else { Some(dst) };
    Ok(Header::new(
        // TODO: Port should presumably remain 0, or be None or something,
        // if this is a Version query or some other non-port related packet
        // type.
        Port(header[0] + 1),
        header[4],
        Pid(header[6]),
        src,
        dst,
        u32::from_le_bytes(
            header[28..32]
                .try_into()
                .expect("can't happen: bytes to u32"),
        ),
    ))
}

/// Command.
pub enum Command {
    Version,
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
    ///
    /// # Errors
    ///
    /// If connecting to the server fails.
    pub fn new(addr: &str) -> Result<AGW> {
        debug!("agw: Creating AGW to {addr}");
        let (tx, rx) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let wstream = TcpStream::connect(addr).map_err(Error::other)?;
        let rstream = wstream.try_clone().map_err(Error::other)?;
        let agw = AGW {
            rx,
            tx: tx2,
            rxqueue: LinkedList::new(),
        };
        // Start reader.
        std::thread::spawn(|| {
            if let Err(e) = Self::reader(rstream, &tx) {
                warn!("TCP socket reader connected to AGWPE ended: {e:?}");
            }
            drop(tx);
        });
        // Start writer.
        std::thread::spawn(|| {
            if let Err(e) = Self::writer(wstream, &rx2) {
                warn!("TCP socket writer connected to AGWPE ended: {e:?}");
            }
            drop(rx2);
        });
        Ok(agw)
    }

    fn send(&mut self, msg: &[u8]) -> Result<()> {
        self.tx.send(msg.to_vec()).map_err(Error::other)?;
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let bytes = packet.serialize()?;
        self.send(&bytes)
    }

    fn sender(&mut self) -> mpsc::Sender<Vec<u8>> {
        self.tx.clone()
    }

    fn writer(mut stream: TcpStream, rx: &mpsc::Receiver<Vec<u8>>) -> Result<()> {
        loop {
            let buf = rx.recv().map_err(Error::other)?;
            stream.write_all(&buf).map_err(Error::other)?;
        }
    }

    fn reader(mut stream: TcpStream, tx: &mpsc::Sender<(Header, Reply)>) -> Result<()> {
        loop {
            let mut header = [0_u8; HEADER_LEN];
            stream.read_exact(&mut header)?;
            let header = parse_header(&header)?;
            let payload = if header.data_len > 0 {
                let mut payload = vec![0; crate::payload_len(header.data_len)?];
                stream.read_exact(&mut payload)?;
                payload
            } else {
                Vec::new()
            };
            let reply = parse_reply(&header, &payload)?;
            trace!("agw: Got reply: {}", reply.description());
            tx.send((header, reply)).map_err(Error::other)?;
        }
    }

    fn rx_enqueue(&mut self, h: Header, r: Reply) {
        const WARN_LIMIT: usize = 10;

        self.rxqueue.push_back((h, r));
        let l = self.rxqueue.len();
        if l > WARN_LIMIT {
            warn!("AGW maxqueue length {l} > {WARN_LIMIT}");
        }
    }

    /// Get the version of the AGW endpoint.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn version(&mut self) -> Result<(u16, u16)> {
        self.send_packet(&Packet::VersionQuery)?;
        loop {
            let (h, r) = self.rx.recv().map_err(Error::other)?;
            match r {
                Reply::Version(maj, min) => return Ok((maj, min)),
                other => self.rx_enqueue(h, other),
            }
        }
    }

    /// Get the number of outstanding frames on a port.
    pub fn frames_outstanding(&mut self, port: Port) -> Result<usize> {
        self.send_packet(&Packet::FramesOutstandingPortQuery(port))?;
        loop {
            let (h, r) = self.rx.recv().map_err(Error::other)?;
            match r {
                Reply::FramesOutstandingPort(p, n) if p == port => return Ok(n),
                other => self.rx_enqueue(h, other),
            }
        }
    }

    /// Get some port info for the AGW endpoint.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn port_info(&mut self) -> Result<PortsInfo> {
        self.send_packet(&Packet::PortInfoQuery)?;
        loop {
            let (h, r) = self.rx.recv().map_err(Error::other)?;
            match r {
                Reply::PortInfo(i) => return Ok(i),
                other => self.rx_enqueue(h, other),
            }
        }
    }

    /// Get port capabilities of the AGW "port".
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn port_cap(&mut self, port: Port) -> Result<PortCaps> {
        let ports = self.port_info()?;
        if !ports.ports.iter().any(|p| p.port == port) {
            return Err(Error::msg(format!("No such port as {port:?}")));
        }
        self.send_packet(&Packet::PortCapQuery(port))?;
        loop {
            let (h, r) = self.rx.recv().map_err(Error::other)?;
            match r {
                Reply::PortCaps(p, i) if p == port => return Ok(i),
                other => self.rx_enqueue(h, other),
            }
        }
    }

    /// Get callsigns heard.
    pub fn callsign_heard(&mut self, port: Port) -> Result<Vec<CallsignHeard>> {
        self.callsign_heard_with_timeout(port, CALLSIGN_HEARD_TIMEOUT)
    }

    /// Get callsigns heard, waiting no longer than `timeout` for all replies.
    ///
    /// AGWPE normally returns twenty `H` frames. Some compatible endpoints,
    /// including Dire Wolf, do not implement this query, so callers that need
    /// a longer wait can opt in without allowing the default method to block
    /// indefinitely.
    ///
    /// # Errors
    ///
    /// If the endpoint does not return all expected replies before `timeout`.
    pub fn callsign_heard_with_timeout(
        &mut self,
        port: Port,
        timeout: Duration,
    ) -> Result<Vec<CallsignHeard>> {
        let mut heard = Vec::new();
        let mut replies = 0;
        let deadline = Instant::now() + timeout;
        self.send_packet(&Packet::CallsignHeardQuery(port))?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (h, r) = self.rx.recv_timeout(remaining).map_err(|error| {
                Error::msg(format!(
                    "timed out waiting for H replies after receiving {replies}: {error}"
                ))
            })?;
            match r {
                Reply::CallsignHeard(p, mut i) if p == port => {
                    heard.append(&mut i);
                    replies += 1;
                    if replies == CALLSIGN_HEARD_REPLIES {
                        return Ok(heard);
                    }
                }
                other => self.rx_enqueue(h, other),
            }
        }
    }

    /// Send UI packet.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn unproto(
        &mut self,
        port: Port,
        pid: Pid,
        src: &Call,
        dst: &Call,
        data: &[u8],
    ) -> Result<()> {
        self.send_packet(&Packet::Unproto {
            port,
            pid,
            src: src.clone(),
            dst: dst.clone(),
            data: data.to_vec(),
        })?;
        Ok(())
    }

    /// Register callsign.
    ///
    /// The specs say that registering the callsign is
    /// mandatory. Direwolf doesn't seem to care, but there it is.
    ///
    /// Presumably needed for incoming connection, but incoming
    /// connections are not tested yet.
    ///
    /// # Errors
    ///
    /// If underlying connection fails.
    pub fn register_callsign(&mut self, port: Port, src: &Call) -> Result<()> {
        debug!("agw: Registering callsign");
        self.send_packet(&Packet::RegisterCallsign(port, src.clone()))?;
        loop {
            let (header, reply) = self.rx.recv().map_err(Error::other)?;
            match reply {
                Reply::CallsignRegistration(true) if header.src.as_ref() == Some(src) => {
                    return Ok(());
                }
                Reply::CallsignRegistration(false) if header.src.as_ref() == Some(src) => {
                    return Err(Error::msg(format!(
                        "callsign registration failed for {src}"
                    )));
                }
                other => self.rx_enqueue(header, other),
            }
        }
    }

    /// Create a new connection.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn connect<'a>(
        &'a mut self,
        port: Port,
        pid: Pid,
        src: &Call,
        dst: &Call,
        via: &[Call],
    ) -> Result<Connection<'a>> {
        if via.is_empty() {
            self.send_packet(&Packet::Connect {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
            })?;
        } else {
            self.send_packet(&Packet::ConnectVia {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
                via: via.to_vec(),
            })?;
        }
        let connect_string;
        loop {
            let (head, r) = self.rx.recv().map_err(Error::other)?;
            // AGWPE uses PID 0x00 on a C confirmation, even when the
            // connection's data PID is 0xf0.
            if head.port != port
                || (head.src.as_ref() != Some(dst))
                || (head.dst.as_ref() != Some(src))
            {
                self.rx_enqueue(head, r);
                continue;
            }
            match r {
                Reply::ConnectionEstablished(i) => {
                    connect_string = i.data.clone();
                    debug!(
                        "agw: Connected from {src} to {dst} with connect string {connect_string}"
                    );
                    break;
                }
                Reply::ConnectionFailed(i) => {
                    return Err(Error::msg(format!("connection failed: {}", i.data)));
                }
                other => self.rx_enqueue(head, other),
            }
        }
        Ok(Connection::new(
            self,
            port,
            connect_string,
            pid,
            src.clone(),
            dst.clone(),
        ))
    }

    fn write_connected(
        &mut self,
        port: Port,
        pid: Pid,
        src: &Call,
        dst: &Call,
        data: &[u8],
    ) -> Result<usize> {
        // TODO: enforce max size?
        let len = data.len();
        if len > 0 {
            self.send_packet(&Packet::Data {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
                data: data.to_vec(),
            })?;
        }
        Ok(data.len())
    }

    fn read_connected(
        &mut self,
        port: Port,
        pid: Pid,
        me: &Call,
        remote: &Call,
    ) -> Result<Vec<u8>> {
        // First check the existing queue.
        for frame in self.rxqueue.iter().enumerate() {
            let (n, (head, payload)) = &frame;
            if head.port != port
                || head.pid != pid
                || (head.src.as_ref() != Some(remote))
                || (head.dst.as_ref() != Some(me))
            {
                continue;
            }
            match payload {
                Reply::ConnectedData(data) => {
                    let ret = data.data.clone();
                    let mut tail = self.rxqueue.split_off(*n);
                    tail.pop_front();
                    self.rxqueue.append(&mut tail);
                    return Ok(ret);
                }
                Reply::Disconnect => {
                    return Err(Error::msg("remote end disconnected"));
                }
                _ => {
                    debug!(
                        "agw: Remote end send unexpected data {}",
                        payload.description()
                    );
                }
            }
        }

        // Next packet not in the queue. Wait.
        loop {
            let (h, r) = self.rx.recv().map_err(Error::other)?;
            match r {
                Reply::ConnectedData(i)
                    if i.port == port && i.pid == pid && i.src == *remote && i.dst == *me =>
                {
                    return Ok(i.data);
                }
                // The spec says that the pid is `0xf0 or 0x00`. At least with
                // direwolf this does not mean that the pid will correspond with
                // the pid of the connection, so we allow any pid value as a
                // disconnect message.
                //
                // Direwolf bug?
                Reply::Disconnect
                    if h.port == port
                        && (h.src.as_ref() == Some(remote))
                        && (h.dst.as_ref() == Some(me)) =>
                {
                    return Err(Error::msg("remote end disconnected"));
                }
                other => self.rx_enqueue(h, other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Call {
        s.parse().unwrap()
    }

    #[test]
    fn parse_reply_keeps_normalized_header_port() {
        let header = Header::new(Port(1), b'y', Pid(0), None, None, 4);
        match parse_reply(&header, &7_u32.to_le_bytes()).unwrap() {
            Reply::FramesOutstandingPort(port, n) => {
                assert_eq!(port, Port(1));
                assert_eq!(n, 7);
            }
            other => panic!("unexpected reply: {other:?}"),
        }

        let header = Header::new(Port(1), b'H', Pid(0), None, None, 1);
        match parse_reply(&header, &[0]).unwrap() {
            Reply::CallsignHeard(port, _) => assert_eq!(port, Port(1)),
            other => panic!("unexpected reply: {other:?}"),
        }

        let header = Header::new(Port(1), b'g', Pid(0), None, None, 12);
        match parse_reply(&header, &[0, 0xff, 30, 10, 63, 10, 4, 0, 0, 0, 0, 0]).unwrap() {
            Reply::PortCaps(port, _) => assert_eq!(port, Port(1)),
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[test]
    fn parse_reply_errors_on_missing_callsigns() {
        let header = Header::new(Port(1), b'D', Pid(0xf0), None, None, 0);
        assert!(parse_reply(&header, &[]).is_err());
    }

    #[test]
    fn parses_connection_failure_as_failure() {
        let remote = call("REMOTE");
        let local = call("LOCAL");
        let header = Header::new(Port(1), b'C', Pid(0), Some(remote), Some(local), 12);

        assert!(matches!(
            parse_reply(&header, b"*** RETRYOUT").unwrap(),
            Reply::ConnectionFailed(_)
        ));
    }

    #[test]
    fn trims_connection_status_terminators() {
        let remote = call("REMOTE");
        let local = call("LOCAL");
        let header = Header::new(Port(1), b'C', Pid(0), Some(remote), Some(local), 33);

        let Reply::ConnectionEstablished(connection) =
            parse_reply(&header, b"*** CONNECTED With Station REMOTE\r\0").unwrap()
        else {
            panic!("expected a successful connection");
        };
        assert_eq!(connection.data, "*** CONNECTED With Station REMOTE");
    }

    #[test]
    fn parses_incoming_connection() {
        let local = call("LOCAL");
        let remote = call("REMOTE");
        let header = Header::new(Port(1), b'C', Pid(0), Some(remote), Some(local), 30);

        assert!(matches!(
            parse_reply(&header, b"*** CONNECTED To Station LOCAL").unwrap(),
            Reply::IncomingConnection(_)
        ));
    }

    #[test]
    fn parses_callsign_heard_entry() {
        assert_eq!(
            parse_callsign_heard(b"REMOTE-1 Mon,21Feb2000 11:14:30\0ignored").unwrap(),
            vec![CallsignHeard {
                call: call("REMOTE-1")
            }]
        );
    }

    #[test]
    fn ignores_empty_callsign_heard_entry() {
        assert_eq!(
            parse_callsign_heard(b"00:00:00 00:00:00\0").unwrap(),
            Vec::<CallsignHeard>::new()
        );
    }

    #[test]
    fn callsign_heard_timeout_prevents_an_indefinite_wait() {
        let (tx, _rx) = mpsc::channel();
        let (_reply_tx, rx) = mpsc::channel();
        let mut agw = AGW {
            rx,
            tx,
            rxqueue: LinkedList::new(),
        };

        assert!(agw
            .callsign_heard_with_timeout(Port(1), Duration::ZERO)
            .is_err());
    }

    #[test]
    fn register_callsign_waits_for_confirmation() {
        let call = call("LOCAL");
        let (rx_tx, rx) = mpsc::channel();
        let (tx, tx_rx) = mpsc::channel();
        let mut agw = AGW {
            rx,
            tx,
            rxqueue: LinkedList::new(),
        };
        rx_tx
            .send((
                Header::new(Port(1), b'X', Pid(0), Some(call.clone()), None, 1),
                Reply::CallsignRegistration(true),
            ))
            .unwrap();

        agw.register_callsign(Port(1), &call).unwrap();
        assert_eq!(
            tx_rx.recv().unwrap(),
            Packet::RegisterCallsign(Port(1), call).serialize().unwrap()
        );
    }

    #[test]
    fn reader_continues_after_a_connection_disconnect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = std::net::TcpStream::connect(addr).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || AGW::reader(stream, &tx));
        let remote = call("REMOTE");
        let local = call("LOCAL");

        writer
            .write_all(
                &Packet::Disconnect {
                    port: Port(1),
                    pid: Pid(0),
                    src: remote,
                    dst: local,
                }
                .serialize()
                .unwrap(),
            )
            .unwrap();
        writer
            .write_all(
                &Packet::VersionReply {
                    major: 2000,
                    minor: 1,
                }
                .serialize()
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        assert!(matches!(rx.recv().unwrap().1, Reply::Disconnect));
        assert!(matches!(rx.recv().unwrap().1, Reply::Version(2000, 1)));
        assert!(reader.join().unwrap().is_err());
    }

    #[test]
    fn read_connected_skips_unrelated_live_data() {
        let me = call("ME");
        let remote = call("REMOTE");
        let other = call("OTHER");
        let (rx_tx, rx) = mpsc::channel();
        let (tx, _tx_rx) = mpsc::channel();
        let mut agw = AGW {
            rx,
            tx,
            rxqueue: LinkedList::new(),
        };

        rx_tx
            .send((
                Header::new(
                    Port(1),
                    b'D',
                    Pid(0xf0),
                    Some(other.clone()),
                    Some(me.clone()),
                    3,
                ),
                Reply::ConnectedData(ConnectedData {
                    port: Port(1),
                    pid: Pid(0xf0),
                    src: other,
                    dst: me.clone(),
                    data: b"bad".to_vec(),
                }),
            ))
            .unwrap();
        rx_tx
            .send((
                Header::new(
                    Port(1),
                    b'D',
                    Pid(0xf0),
                    Some(remote.clone()),
                    Some(me.clone()),
                    2,
                ),
                Reply::ConnectedData(ConnectedData {
                    port: Port(1),
                    pid: Pid(0xf0),
                    src: remote.clone(),
                    dst: me.clone(),
                    data: b"ok".to_vec(),
                }),
            ))
            .unwrap();

        assert_eq!(
            agw.read_connected(Port(1), Pid(0xf0), &me, &remote)
                .unwrap(),
            b"ok"
        );
        assert_eq!(agw.rxqueue.len(), 1);
    }

    #[test]
    fn read_connected_accepts_a_zero_pid_disconnect() {
        let me = call("ME");
        let remote = call("REMOTE");
        let (rx_tx, rx) = mpsc::channel();
        let (tx, _tx_rx) = mpsc::channel();
        let mut agw = AGW {
            rx,
            tx,
            rxqueue: LinkedList::new(),
        };

        rx_tx
            .send((
                Header::new(
                    Port(1),
                    b'd',
                    Pid(0),
                    Some(remote.clone()),
                    Some(me.clone()),
                    0,
                ),
                Reply::Disconnect,
            ))
            .unwrap();

        assert!(agw
            .read_connected(Port(1), Pid(0xf0), &me, &remote)
            .is_err());
    }
}

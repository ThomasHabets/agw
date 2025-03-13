use crate::HEADER_LEN;
use crate::{Call, Header, Packet};
use anyhow::{Error, Result};
use log::{debug, trace, warn};
use std::collections::LinkedList;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;

// TODO: get rid of Reply struct. It's just a subset of Packet.

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
            Reply::Disconnect => "Disconnect".to_string(),
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
            Reply::MonitorConnected(x) => format!("Connected packet len {}", x.len()),
            Reply::MonitorSupervisory(x) => format!("Supervisory packet len {}", x.len()),
            Reply::HeardStations(s) => format!("Heard stations: {s}"),
            Reply::Unknown(h, data) => format!("Unknown reply: header={h:?} data={data:?}"),
        }
    }
}

fn parse_reply(header: &Header, data: &[u8]) -> Result<Reply> {
    // TODO: confirm data len, since most replies will have fixed size.
    Ok(match header.data_kind() {
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
            let bytes_per_2min =
                u32::from_le_bytes(data[8..12].try_into().expect("can't happen: bytes to u32"));

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
        b'y' => Reply::FramesOutstandingPort(u32::from_le_bytes(
            data[0..4].try_into().expect("can't happen: bytes to u32"),
        )),
        b'Y' => Reply::FramesOutstandingConnection(u32::from_le_bytes(
            data[0..4].try_into().expect("can't happen: bytes to u32"),
        )),
        b'H' => Reply::HeardStations(std::str::from_utf8(data)?.to_string()),
        b'I' => Reply::MonitorConnected(data.to_vec()),
        b'S' => Reply::MonitorSupervisory(data.to_vec()),
        b'K' => Reply::Raw(data.to_vec()),
        _ => Reply::Unknown(header.clone(), data.to_vec()),
    })
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
    pub fn data(&self, data: &[u8]) -> Result<Vec<u8>> {
        Ok(Packet::Data {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
            data: data.to_vec(),
        }
        .serialize())
    }
    pub fn disconnect(&self) -> Vec<u8> {
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
    port: u8,
    connect_string: String,
    pid: u8,
    src: Call,
    dst: Call,
    agw: &'a mut AGW,
    disconnected: bool,
}

impl<'a> Connection<'a> {
    fn new(
        agw: &'a mut AGW,
        port: u8,
        connect_string: String,
        pid: u8,
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
    pub fn connect_string(&self) -> &str {
        &self.connect_string
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
            self.agw.send(
                &Packet::Disconnect {
                    port: self.port,
                    pid: self.pid,
                    src: self.src.clone(),
                    dst: self.dst.clone(),
                }
                .serialize(),
            )?;
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

pub fn parse_header(header: &[u8; HEADER_LEN]) -> Result<Header> {
    let src = Call::from_bytes(&header[8..18])?;
    let src = if src.is_empty() { None } else { Some(src) };
    let dst = Call::from_bytes(&header[18..28])?;
    let dst = if dst.is_empty() { None } else { Some(dst) };
    Ok(Header::new(
        header[0],
        header[4],
        header[6],
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
    pub fn new(addr: &str) -> Result<AGW> {
        debug!("Creating AGW to {addr}");
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
            // TODO: do full write.
            let _ = stream.write(&buf)?;
        }
    }

    fn reader(mut stream: TcpStream, tx: mpsc::Sender<(Header, Reply)>) -> Result<()> {
        loop {
            let mut header = [0_u8; HEADER_LEN];
            stream.read_exact(&mut header)?;
            let header = parse_header(&header)?;
            let payload = if header.data_len() > 0 {
                let mut payload = vec![0; header.data_len() as usize];
                stream.read_exact(&mut payload)?;
                payload
            } else {
                Vec::new()
            };
            let reply = parse_reply(&header, &payload)?;
            trace!("Got reply: {}", reply.description());
            let done = matches!(reply, Reply::Disconnect);
            tx.send((header, reply))?;
            if done {
                break Ok(());
            }
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
        self.send(&Packet::VersionQuery.serialize())?;
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
        self.send(&Packet::PortInfo(port).serialize())?;
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
        self.send(&Packet::PortCap(port).serialize())?;
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
        self.send(
            &Packet::Unproto {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
                data: data.to_vec(),
            }
            .serialize(),
        )?;
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
        self.send(&Packet::RegisterCallsign(port, pid, src.clone()).serialize())?;
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
        if via.is_empty() {
            self.send(
                &Packet::Connect {
                    port,
                    pid,
                    src: src.clone(),
                    dst: dst.clone(),
                }
                .serialize(),
            )?;
        } else {
            self.send(
                &Packet::ConnectVia {
                    port,
                    pid,
                    src: src.clone(),
                    dst: dst.clone(),
                    via: via.to_vec(),
                }
                .serialize(),
            )?;
            todo!();
        }
        let connect_string;
        loop {
            let (head, r) = self.rx.recv()?;
            if head.src().as_ref().map_or(true, |x| x != dst)
                || head.dst().as_ref().map_or(true, |x| x != src)
            {
                //eprintln!("Got packet not for us");
                continue;
            }
            match r {
                Reply::Connected(i) => {
                    connect_string = i.clone();
                    debug!("Connected from {src} to {dst} with connect string {i}");
                    break;
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
        port: u8,
        pid: u8,
        src: &Call,
        dst: &Call,
        data: &[u8],
    ) -> Result<usize> {
        // TODO: enforce max size?
        let len = data.len();
        if len > 0 {
            self.send(
                &Packet::Data {
                    port,
                    pid,
                    src: src.clone(),
                    dst: dst.clone(),
                    data: data.to_vec(),
                }
                .serialize(),
            )?;
        }
        Ok(data.len())
    }

    fn read_connected(&mut self, me: &Call, remote: &Call) -> Result<Vec<u8>> {
        // First check the existing queue.
        for frame in self.rxqueue.iter().enumerate() {
            let (n, (head, payload)) = &frame;
            if head.src().as_ref().map_or(true, |x| x != remote)
                || head.dst().as_ref().map_or(true, |x| x != me)
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

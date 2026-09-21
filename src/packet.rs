use log::debug;
use std::fmt::Write;

use crate::v1::{Baud, PortCaps, PortInfo, PortsInfo};
use crate::{Call, Header};
use crate::{Error, Result};

const CMD_VERSION: u8 = b'R';
const CMD_FRAMES_OUTSTANDING_PORT: u8 = b'y';
const CMD_CONNECT: u8 = b'C';
const CMD_CONNECT_VIA: u8 = b'v';
const CMD_DISCONNECT: u8 = b'd';
const CMD_REGISTER_CALLSIGN: u8 = b'X';
const CMD_DATA: u8 = b'D';
const CMD_UNPROTO: u8 = b'M';
const CMD_PORT_INFO: u8 = b'G';
const CMD_CALLSIGN_HEARD: u8 = b'H';
const CMD_PORT_CAP: u8 = b'g';

/// Port number.
#[derive(Copy, Clone, Debug, PartialEq, Hash, Eq)]
pub struct Port(pub u8);

/// PID number.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Pid(pub u8);

/// A digipeater in a connect-via route.
///
/// A seen hop has already repeated the frame. AX.25 represents seen hops as
/// a contiguous prefix of the route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViaHop {
    call: Call,
    seen: bool,
}

impl ViaHop {
    /// Create an unseen digipeater hop.
    #[must_use]
    pub fn new(call: Call) -> Self {
        Self { call, seen: false }
    }

    /// Create a digipeater hop that has already repeated the frame.
    #[must_use]
    pub fn seen(call: Call) -> Self {
        Self { call, seen: true }
    }

    /// Callsign of this digipeater.
    #[must_use]
    pub fn callsign(&self) -> &Call {
        &self.call
    }

    /// Whether this digipeater has already repeated the frame.
    #[must_use]
    pub fn is_seen(&self) -> bool {
        self.seen
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Packet {
    /// Application: Version query.
    VersionQuery,

    /// Application: Ask outstanding frames.
    FramesOutstandingPortQuery(Port),

    /// AGWPE: Outstanding frame count report.
    FramesOutstandingPortReply(Port, usize),

    /// AGWPE: Version reply.
    VersionReply {
        major: u16,
        minor: u16,
    },

    /// AGWPE: Callsign registration reply.
    RegisterCallsignReply {
        port: Port,
        call: Call,
        success: bool,
    },

    /// Application: Port capability query.
    PortCapQuery(Port),

    /// AGWPE: Port capability reply.
    PortCapReply {
        port: Port,
        caps: PortCaps,
    },

    /// Application: List heard callsigns.
    CallsignHeardQuery(Port),

    /// AGWPE: Callsigns heard reply.
    ///
    /// The full AGW payload contains a NUL-terminated text area followed by
    /// two 16-byte SYSTEMTIME values. Higher-level APIs parse those values
    /// into [`crate::CallsignHeardTimestamp`].
    CallsignHeardReply {
        port: Port,
        data: Vec<u8>,
    },

    /// Application: Port info query.
    PortInfoQuery,

    /// AGWPE: Port info reply.
    PortInfoReply(PortsInfo),

    RegisterCallsign(Port, Call),
    Connect {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
    },
    ConnectVia {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
        via: Vec<Call>,
    },
    /// A connect-via request with AX.25 seen state for each digipeater.
    ConnectViaMarked {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
        via: Vec<ViaHop>,
    },
    IncomingConnect {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
    },
    ConnectionEstablished {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
    },
    /// AGWPE reported that a connection attempt failed.
    ConnectionFailed {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
        message: String,
    },
    Disconnect {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
    },
    Unproto {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
        data: Vec<u8>,
    },
    Data {
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
        data: Vec<u8>,
    },
    /// A frame type the crate does not model yet.
    ///
    /// Its header fields and transparent payload are retained so proxies and
    /// applications can continue operating when AGWPE sends another standard
    /// frame type.
    Opaque {
        port: Port,
        pid: Pid,
        data_kind: u8,
        src: Option<Call>,
        dst: Option<Call>,
        data: Vec<u8>,
    },
    // FramesOutstandingConnection(u32), // Y
    // HeardStations(String) // H
    // MonitorConnected(Vec<u8>) // I
    // MonitorSupervisory(Vec<u8>) // S
    // Raw() // R.
    // Unknown
}

const MAX_CONNECT_VIA_HOPS: usize = 7;

fn serialize_connect_via(
    port: Port,
    pid: Pid,
    src: &Call,
    dst: &Call,
    hops: Vec<[u8; 10]>,
) -> Result<Vec<u8>> {
    if hops.is_empty() {
        return Err(Error::msg("connect via requires at least one hop"));
    }
    if hops.len() > MAX_CONNECT_VIA_HOPS {
        return Err(Error::msg(format!(
            "tried to connect through too many hops: {} > {MAX_CONNECT_VIA_HOPS}",
            hops.len()
        )));
    }

    let mut data = Vec::with_capacity(1 + hops.len() * 10);
    data.push(u8::try_from(hops.len())?);
    for hop in hops {
        data.extend_from_slice(&hop);
    }
    let header = Header::new(
        port,
        CMD_CONNECT_VIA,
        pid,
        Some(src.clone()),
        Some(dst.clone()),
        u32::try_from(data.len()).expect("connect via payload fits in u32"),
    )
    .serialize();
    Ok([header, data].concat())
}

fn marked_connect_via_hops(via: &[ViaHop]) -> Result<Vec<[u8; 10]>> {
    let mut saw_unseen = false;
    let mut hops = Vec::with_capacity(via.len());

    for hop in via {
        if hop.is_seen() && saw_unseen {
            return Err(Error::msg(
                "seen connect-via hops must form a contiguous route prefix",
            ));
        }
        saw_unseen |= !hop.is_seen();

        let call = hop.callsign().as_bytes();
        let call_len = call
            .iter()
            .position(|&byte| byte == 0)
            .expect("callsigns are NUL terminated");
        if hop.is_seen() && call_len == call.len() - 1 {
            return Err(Error::msg(format!(
                "seen connect-via callsign '{}' has no room for marker",
                hop.callsign()
            )));
        }

        let mut field = [0; 10];
        field[..call_len].copy_from_slice(&call[..call_len]);
        if hop.is_seen() {
            field[call_len] = b'*';
        }
        hops.push(field);
    }
    Ok(hops)
}

impl Packet {
    /// Serialize packet for AGW connection.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::missing_panics_doc)]
    pub fn serialize(&self) -> Result<Vec<u8>> {
        if let Some(port) = match self {
            Packet::VersionQuery
            | Packet::VersionReply { .. }
            | Packet::PortInfoQuery
            | Packet::PortInfoReply(_) => None,
            Packet::FramesOutstandingPortQuery(port)
            | Packet::FramesOutstandingPortReply(port, _)
            | Packet::RegisterCallsignReply { port, .. }
            | Packet::Connect { port, .. }
            | Packet::IncomingConnect { port, .. }
            | Packet::ConnectionEstablished { port, .. }
            | Packet::ConnectionFailed { port, .. }
            | Packet::ConnectVia { port, .. }
            | Packet::ConnectViaMarked { port, .. }
            | Packet::RegisterCallsign(port, _)
            | Packet::Disconnect { port, .. }
            | Packet::Data { port, .. }
            | Packet::Unproto { port, .. }
            | Packet::CallsignHeardQuery(port)
            | Packet::CallsignHeardReply { port, .. }
            | Packet::PortCapQuery(port)
            | Packet::PortCapReply { port, .. }
            | Packet::Opaque { port, .. } => Some(*port),
        } {
            if port.0 == 0 {
                return Err(Error::msg("AGW port numbers start at 1"));
            }
        }
        Ok(match self {
            Packet::VersionQuery => {
                Header::new(Port(0), CMD_VERSION, Pid(0), None, None, 0).serialize()
            }
            Packet::FramesOutstandingPortQuery(port) => {
                Header::new(*port, CMD_FRAMES_OUTSTANDING_PORT, Pid(0), None, None, 0).serialize()
            }
            Packet::FramesOutstandingPortReply(port, n) => [
                Header::new(*port, CMD_FRAMES_OUTSTANDING_PORT, Pid(0), None, None, 4).serialize(),
                u32::try_from(*n)
                    .expect("can't happen. Has to fit")
                    .to_le_bytes()
                    .to_vec(),
            ]
            .concat(),
            Packet::VersionReply { major, minor } => {
                let data = vec![
                    u8::try_from(*major & 0xff).expect("can't happen"),
                    (*major >> 8) as u8,
                    0,
                    0,
                    u8::try_from(*minor & 0xff).expect("can't happen"),
                    (*minor >> 8) as u8,
                    0,
                    0,
                ];
                [
                    Header::new(
                        Port(0),
                        CMD_VERSION,
                        Pid(0),
                        None,
                        None,
                        u32::try_from(data.len()).expect("can't happen"),
                    )
                    .serialize(),
                    data,
                ]
                .concat()
            }
            Packet::RegisterCallsignReply {
                port,
                call,
                success,
            } => [
                Header::new(
                    *port,
                    CMD_REGISTER_CALLSIGN,
                    Pid(0),
                    Some(call.clone()),
                    None,
                    1,
                )
                .serialize(),
                vec![u8::from(*success)],
            ]
            .concat(),
            Packet::Connect {
                port,
                pid,
                src,
                dst,
            } => {
                Header::new(*port, b'C', *pid, Some(src.clone()), Some(dst.clone()), 0).serialize()
            }
            Packet::IncomingConnect {
                port,
                pid,
                src,
                dst,
            } => [
                Header::new(
                    *port,
                    CMD_CONNECT,
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(
                        format!("*** CONNECTED To Station {}", src.as_str())
                            .as_bytes()
                            .len(),
                    )
                    .expect("can't happen"),
                )
                .serialize(),
                format!("*** CONNECTED To Station {}", src.as_str())
                    .as_bytes()
                    .to_vec(),
            ]
            .concat(),
            Packet::ConnectionEstablished {
                port,
                pid,
                src,
                dst,
            } => [
                Header::new(
                    *port,
                    CMD_CONNECT,
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(
                        format!("*** CONNECTED With Station {}", src.as_str())
                            .as_bytes()
                            .len(),
                    )
                    .expect("can't happen"),
                )
                .serialize(),
                format!("*** CONNECTED With Station {}", src.as_str())
                    .as_bytes()
                    .to_vec(),
            ]
            .concat(),
            Packet::ConnectionFailed {
                port,
                pid,
                src,
                dst,
                message,
            } => [
                Header::new(
                    *port,
                    CMD_CONNECT,
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(message.len()).expect("can't happen"),
                )
                .serialize(),
                message.as_bytes().to_vec(),
            ]
            .concat(),
            Packet::ConnectVia {
                port,
                pid,
                src,
                dst,
                via,
            } => serialize_connect_via(
                *port,
                *pid,
                src,
                dst,
                via.iter()
                    .map(|call| {
                        let mut field = [0; 10];
                        field.copy_from_slice(call.as_bytes());
                        field
                    })
                    .collect(),
            )?,
            Packet::ConnectViaMarked {
                port,
                pid,
                src,
                dst,
                via,
            } => serialize_connect_via(*port, *pid, src, dst, marked_connect_via_hops(via)?)?,
            Packet::RegisterCallsign(port, src) => Header::new(
                *port,
                CMD_REGISTER_CALLSIGN,
                Pid(0),
                Some(src.clone()),
                None,
                0,
            )
            .serialize(),
            Packet::Disconnect {
                port,
                pid,
                src,
                dst,
            } => {
                Header::new(*port, b'd', *pid, Some(src.clone()), Some(dst.clone()), 0).serialize()
            }
            Packet::Data {
                port,
                pid,
                src,
                dst,
                data,
            } => [
                Header::new(
                    *port,
                    CMD_DATA,
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(data.len()).expect("TODO: return an error"),
                )
                .serialize(),
                data.clone(),
            ]
            .concat(),
            Packet::Unproto {
                port,
                pid,
                src,
                dst,
                data,
            } => [
                Header::new(
                    *port,
                    CMD_UNPROTO,
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(data.len()).expect("TODO: return err or something"),
                )
                .serialize(),
                data.clone(),
            ]
            .concat(),
            Packet::PortInfoQuery => {
                Header::new(Port(0), CMD_PORT_INFO, Pid(0), None, None, 0).serialize()
            }
            Packet::PortInfoReply(info) => {
                let mut payload = format!("{};", info.count);
                for port in &info.ports {
                    let _ = write!(payload, "Port{} {};", port.port.0, port.descr);
                }
                payload.push('\0');
                [
                    Header::new(
                        Port(0),
                        CMD_PORT_INFO,
                        Pid(0),
                        None,
                        None,
                        u32::try_from(payload.len()).expect("can't happen"),
                    )
                    .serialize(),
                    payload.into_bytes(),
                ]
                .concat()
            }
            Packet::CallsignHeardQuery(port) => {
                Header::new(*port, CMD_CALLSIGN_HEARD, Pid(0), None, None, 0).serialize()
            }
            Packet::CallsignHeardReply { port, data } => [
                Header::new(
                    *port,
                    CMD_CALLSIGN_HEARD,
                    Pid(0),
                    None,
                    None,
                    u32::try_from(data.len()).expect("can't happen"),
                )
                .serialize(),
                data.clone(),
            ]
            .concat(),
            Packet::PortCapQuery(port) => {
                Header::new(*port, CMD_PORT_CAP, Pid(0), None, None, 0).serialize()
            }
            Packet::PortCapReply { port, caps } => [
                Header::new(*port, CMD_PORT_CAP, Pid(0), None, None, 12).serialize(),
                vec![
                    match caps.rate {
                        Baud::Unknown => 0xff,
                        Baud::B1200 => 0,
                        Baud::B2400 => 1,
                        Baud::B4800 => 2,
                        Baud::B9600 => 3,
                    },
                    caps.traffic_level.unwrap_or(0xff),
                    caps.tx_delay,
                    caps.tx_tail,
                    caps.persist,
                    caps.slot_time,
                    caps.max_frame,
                    caps.active_connections,
                ],
                caps.bytes_per_2min.to_le_bytes().to_vec(),
            ]
            .concat(),
            Packet::Opaque {
                port,
                pid,
                data_kind,
                src,
                dst,
                data,
            } => [
                Header::new(
                    *port,
                    *data_kind,
                    *pid,
                    src.clone(),
                    dst.clone(),
                    u32::try_from(data.len()).expect("can't happen"),
                )
                .serialize(),
                data.clone(),
            ]
            .concat(),
        })
    }
    #[allow(clippy::too_many_lines)]
    pub fn parse(header: &Header, data: &[u8]) -> Result<Packet> {
        Ok(match header.data_kind {
            CMD_VERSION => {
                if data.is_empty() {
                    Packet::VersionQuery
                } else if data.len() == 8 {
                    #[allow(clippy::missing_panics_doc)]
                    let major = u16::from_le_bytes(
                        data[0..2]
                            .try_into()
                            .expect("can't happen: two bytes can't be made into u16?"),
                    );
                    #[allow(clippy::missing_panics_doc)]
                    let minor = u16::from_le_bytes(
                        data[4..6]
                            .try_into()
                            .expect("can't happen: two bytes can't be made into u16?"),
                    );
                    Packet::VersionReply { major, minor }
                } else {
                    return Err(Error::msg(format!(
                        "version packet had wrong length {}, {data:?}",
                        header.data_kind
                    )));
                }
            }
            CMD_CONNECT => {
                let src = header
                    .src
                    .clone()
                    .ok_or(Error::msg("connect missing src"))?;
                let dst = header
                    .dst
                    .clone()
                    .ok_or(Error::msg("connect missing src"))?;
                if data.is_empty() {
                    debug!("agw: Got Connect {src:?} to {dst:?}");
                    Packet::Connect {
                        port: header.port,
                        pid: header.pid,
                        src,
                        dst,
                    }
                } else {
                    let s = String::from_utf8(data.to_vec()).map_err(Error::other)?;
                    if s.starts_with("*** CONNECTED WITH")
                        || s.starts_with("*** CONNECTED With ")
                        || s.starts_with("*** CONNECTED With Station ")
                    {
                        debug!("agw: Got ConnectionEstablished {s}");
                        Packet::ConnectionEstablished {
                            port: header.port,
                            pid: header.pid,
                            src,
                            dst,
                        }
                    } else if s.starts_with("*** CONNECTED To Station") {
                        debug!("agw: Got IncomingConnect {s}");
                        Packet::IncomingConnect {
                            port: header.port,
                            pid: header.pid,
                            src,
                            dst,
                        }
                    } else {
                        // Is a `C` with a nonstandard message really the way
                        // connection failed is communicated? Seems to me that
                        // it should be a `d` frame, no?
                        debug!("agw: Got ConnectionFailed {s}");
                        Packet::ConnectionFailed {
                            port: header.port,
                            pid: header.pid,
                            src,
                            dst,
                            message: s,
                        }
                    }
                }
            }
            CMD_CONNECT_VIA => {
                let src = header
                    .src
                    .clone()
                    .ok_or(Error::msg("connect via missing src"))?;
                let dst = header
                    .dst
                    .clone()
                    .ok_or(Error::msg("connect via missing dst"))?;
                let Some(&nhops) = data.first() else {
                    return Err(Error::msg("connect via missing hop count"));
                };
                let expected = 1 + usize::from(nhops) * 10;
                if data.len() != expected {
                    return Err(Error::msg(format!(
                        "connect via had wrong length {} != {expected}",
                        data.len()
                    )));
                }
                let mut via = Vec::with_capacity(usize::from(nhops));
                let mut marked_via = Vec::with_capacity(usize::from(nhops));
                let mut has_seen_hop = false;
                for chunk in data[1..].as_chunks::<10>().0 {
                    let end = chunk
                        .iter()
                        .position(|&byte| byte == 0)
                        .unwrap_or(chunk.len());
                    let (call, seen) = match chunk[..end].strip_suffix(b"*") {
                        Some(call) => (Call::from_bytes(call)?, true),
                        None => (Call::from_bytes(&chunk[..end])?, false),
                    };
                    has_seen_hop |= seen;
                    via.push(call.clone());
                    marked_via.push(if seen {
                        ViaHop::seen(call)
                    } else {
                        ViaHop::new(call)
                    });
                }
                if has_seen_hop {
                    marked_connect_via_hops(&marked_via)?;
                    debug!("agw: Got marked ConnectVia from {src:?} to {dst:?} via {marked_via:?}");
                    Packet::ConnectViaMarked {
                        port: header.port,
                        pid: header.pid,
                        src,
                        dst,
                        via: marked_via,
                    }
                } else {
                    debug!("agw: Got ConnectVia from {src:?} to {dst:?} via {via:?}");
                    Packet::ConnectVia {
                        port: header.port,
                        pid: header.pid,
                        src,
                        dst,
                        via,
                    }
                }
            }
            CMD_DISCONNECT => Packet::Disconnect {
                port: header.port,
                pid: header.pid,
                src: header
                    .src
                    .clone()
                    .ok_or(Error::msg("disconnect missing src"))?,
                dst: header
                    .dst
                    .clone()
                    .ok_or(Error::msg("disconnect missing dst"))?,
            },
            CMD_UNPROTO => Packet::Unproto {
                port: header.port,
                pid: header.pid,
                src: header
                    .src
                    .clone()
                    .ok_or(Error::msg("unproto with missing src"))?,
                dst: header
                    .dst
                    .clone()
                    .ok_or(Error::msg("unproto with missing dst"))?,
                data: data.to_vec(),
            },
            CMD_DATA => Packet::Data {
                port: header.port,
                pid: header.pid,
                src: header
                    .src
                    .clone()
                    .ok_or(Error::msg("data with missing src"))?,
                dst: header
                    .dst
                    .clone()
                    .ok_or(Error::msg("data with missing dst"))?,
                data: data.to_vec(),
            },
            CMD_REGISTER_CALLSIGN => {
                let call = header
                    .src
                    .clone()
                    .ok_or(Error::msg("callsign packet missing src"))?;
                if data.is_empty() {
                    Packet::RegisterCallsign(header.port, call)
                } else if data.len() == 1 {
                    Packet::RegisterCallsignReply {
                        port: header.port,
                        call,
                        success: data[0] != 0,
                    }
                } else {
                    return Err(Error::msg(format!(
                        "callsign registration packet had wrong length {}, {data:?}",
                        data.len()
                    )));
                }
            }
            CMD_FRAMES_OUTSTANDING_PORT => {
                if data.is_empty() {
                    Packet::FramesOutstandingPortQuery(header.port)
                } else if data.len() == 4 {
                    Packet::FramesOutstandingPortReply(
                        header.port,
                        usize::try_from(u32::from_le_bytes(
                            data.try_into().expect("can't happen: bytes to u32"),
                        ))
                        .expect("TODO: some error"),
                    )
                } else {
                    return Err(Error::msg(format!(
                        "frames outstanding packet had wrong length {}, {data:?}",
                        data.len()
                    )));
                }
            }
            CMD_PORT_INFO => {
                if data.is_empty() {
                    Packet::PortInfoQuery
                } else {
                    let s = std::str::from_utf8(data).map_err(Error::other)?;
                    let mut parts = s.splitn(2, ';');
                    let count = parts
                        .next()
                        .ok_or(Error::msg("port info reply missing count"))?
                        .parse()
                        .map_err(Error::other)?;
                    let ports = parts
                        .next()
                        .ok_or(Error::msg("port info reply missing ports"))?
                        .split(';')
                        .map(std::string::ToString::to_string)
                        .filter(|s| !s.is_empty() && s != "\0")
                        .map(|entry| {
                            let entry = entry.trim_end_matches('\0');
                            let rest = entry
                                .strip_prefix("Port")
                                .ok_or(Error::msg(format!("bad port line {entry:?}")))?;
                            let split = rest
                                .find(char::is_whitespace)
                                .ok_or(Error::msg(format!("bad port line {entry:?}")))?;
                            let port = Port(rest[..split].parse().map_err(Error::other)?);
                            Ok::<_, Error>(PortInfo {
                                port,
                                descr: rest[split..].trim_start().to_string(),
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Packet::PortInfoReply(PortsInfo { count, ports })
                }
            }
            CMD_CALLSIGN_HEARD => {
                if data.is_empty() {
                    Packet::CallsignHeardQuery(header.port)
                } else {
                    Packet::CallsignHeardReply {
                        port: header.port,
                        data: data.to_vec(),
                    }
                }
            }
            CMD_PORT_CAP => {
                if data.is_empty() {
                    Packet::PortCapQuery(header.port)
                } else if data.len() == 12 {
                    Packet::PortCapReply {
                        port: header.port,
                        caps: PortCaps {
                            rate: match data[0] {
                                0 => Baud::B1200,
                                1 => Baud::B2400,
                                2 => Baud::B4800,
                                3 => Baud::B9600,
                                _ => Baud::Unknown,
                            },
                            traffic_level: if data[1] == 0xff { None } else { Some(data[1]) },
                            tx_delay: data[2],
                            tx_tail: data[3],
                            persist: data[4],
                            slot_time: data[5],
                            max_frame: data[6],
                            active_connections: data[7],
                            bytes_per_2min: u32::from_le_bytes(
                                data[8..12].try_into().expect("can't happen: bytes to u32"),
                            ),
                        },
                    }
                } else {
                    return Err(Error::msg(format!(
                        "port cap reply had wrong length {}, {data:?}",
                        data.len()
                    )));
                }
            }
            _ => Packet::Opaque {
                port: header.port,
                pid: header.pid,
                data_kind: header.data_kind,
                src: header.src.clone(),
                dst: header.dst.clone(),
                data: data.to_vec(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_data_serializes_as_zero_length_frame() {
        let src: Call = "SRC".parse().unwrap();
        let dst: Call = "DST".parse().unwrap();
        let packet = Packet::Data {
            port: Port(1),
            pid: Pid(0xf0),
            src,
            dst,
            data: Vec::new(),
        };

        let bytes = packet.serialize().unwrap();
        assert_eq!(bytes.len(), crate::HEADER_LEN);
        assert_eq!(bytes[4], CMD_DATA);
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 0);

        let header: [u8; crate::HEADER_LEN] = bytes.try_into().unwrap();
        let header = crate::parse_header(&header).unwrap();
        assert_eq!(Packet::parse(&header, &[]).unwrap(), packet);
    }

    #[test]
    fn connected_data_preserves_packet_boundaries() {
        let src: Call = "SRC".parse().unwrap();
        let dst: Call = "DST".parse().unwrap();
        let data = vec![b'x'; 201];
        let bytes = Packet::Data {
            port: Port(1),
            pid: Pid(0xf0),
            src,
            dst,
            data: data.clone(),
        }
        .serialize()
        .unwrap();

        assert_eq!(bytes.len(), crate::HEADER_LEN + data.len());
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 201);
        assert_eq!(&bytes[crate::HEADER_LEN..], data);
    }

    #[test]
    fn parses_standard_connection_confirmation() {
        let src: Call = "REMOTE".parse().unwrap();
        let dst: Call = "LOCAL".parse().unwrap();
        let header = Header::new(
            Port(1),
            CMD_CONNECT,
            Pid(0),
            Some(src.clone()),
            Some(dst.clone()),
            27,
        );

        assert_eq!(
            Packet::parse(&header, b"*** CONNECTED With REMOTE\r\0").unwrap(),
            Packet::ConnectionEstablished {
                port: Port(1),
                pid: Pid(0),
                src,
                dst,
            }
        );
    }

    #[test]
    fn preserves_connection_failure_message() {
        let src: Call = "REMOTE".parse().unwrap();
        let dst: Call = "LOCAL".parse().unwrap();
        let header = Header::new(
            Port(1),
            CMD_CONNECT,
            Pid(0),
            Some(src.clone()),
            Some(dst.clone()),
            12,
        );

        assert_eq!(
            Packet::parse(&header, b"*** RETRYOUT").unwrap(),
            Packet::ConnectionFailed {
                port: Port(1),
                pid: Pid(0),
                src,
                dst,
                message: "*** RETRYOUT".into(),
            }
        );
    }

    #[test]
    fn rejects_invalid_connect_via_paths() {
        let src: Call = "LOCAL".parse().unwrap();
        let dst: Call = "REMOTE".parse().unwrap();
        let packet = Packet::ConnectVia {
            port: Port(1),
            pid: Pid(0xf0),
            src: src.clone(),
            dst: dst.clone(),
            via: Vec::new(),
        };
        assert!(packet.serialize().is_err());

        let hop: Call = "WIDE1-1".parse().unwrap();
        let packet = Packet::ConnectVia {
            port: Port(1),
            pid: Pid(0xf0),
            src,
            dst,
            via: vec![hop; 8],
        };
        assert!(packet.serialize().is_err());
    }

    #[test]
    fn rejects_zero_port_numbers() {
        let src: Call = "LOCAL".parse().unwrap();
        let dst: Call = "REMOTE".parse().unwrap();
        let packet = Packet::Connect {
            port: Port(0),
            pid: Pid(0xf0),
            src,
            dst,
        };

        assert_eq!(
            packet.serialize().unwrap_err().to_string(),
            "An error occurred: AGW port numbers start at 1"
        );
    }

    #[test]
    fn serializes_marked_connect_via_hops() {
        let src: Call = "LOCAL".parse().unwrap();
        let dst: Call = "REMOTE".parse().unwrap();
        let seen: Call = "WIDE1-1".parse().unwrap();
        let unseen: Call = "WIDE2-2".parse().unwrap();
        let packet = Packet::ConnectViaMarked {
            port: Port(1),
            pid: Pid(0xf0),
            src,
            dst,
            via: vec![ViaHop::seen(seen), ViaHop::new(unseen)],
        };

        let bytes = packet.serialize().unwrap();
        assert_eq!(
            &bytes[crate::HEADER_LEN..],
            b"\x02WIDE1-1*\0\0WIDE2-2\0\0\0"
        );
    }

    #[test]
    fn parses_marked_connect_via_hops() {
        let src: Call = "LOCAL".parse().unwrap();
        let dst: Call = "REMOTE".parse().unwrap();
        let header = Header::new(
            Port(1),
            CMD_CONNECT_VIA,
            Pid(0xf0),
            Some(src.clone()),
            Some(dst.clone()),
            21,
        );

        assert_eq!(
            Packet::parse(&header, b"\x02WIDE1-1*\0\0WIDE2-2\0\0\0").unwrap(),
            Packet::ConnectViaMarked {
                port: Port(1),
                pid: Pid(0xf0),
                src,
                dst,
                via: vec![
                    ViaHop::seen("WIDE1-1".parse().unwrap()),
                    ViaHop::new("WIDE2-2".parse().unwrap()),
                ],
            }
        );
    }

    #[test]
    fn serializes_an_all_seen_connect_via_route() {
        let src: Call = "LOCAL".parse().unwrap();
        let dst: Call = "REMOTE".parse().unwrap();
        let packet = Packet::ConnectViaMarked {
            port: Port(1),
            pid: Pid(0xf0),
            src,
            dst,
            via: vec![
                ViaHop::seen("WIDE1-1".parse().unwrap()),
                ViaHop::seen("WIDE2-2".parse().unwrap()),
            ],
        };

        let bytes = packet.serialize().unwrap();
        assert_eq!(&bytes[crate::HEADER_LEN..], b"\x02WIDE1-1*\0\0WIDE2-2*\0\0");
    }

    #[test]
    fn rejects_invalid_marked_connect_via_paths() {
        let src: Call = "LOCAL".parse().unwrap();
        let dst: Call = "REMOTE".parse().unwrap();
        let first: Call = "WIDE1-1".parse().unwrap();
        let second: Call = "WIDE2-2".parse().unwrap();
        let packet = Packet::ConnectViaMarked {
            port: Port(1),
            pid: Pid(0xf0),
            src: src.clone(),
            dst: dst.clone(),
            via: vec![ViaHop::new(first), ViaHop::seen(second)],
        };
        assert!(packet.serialize().is_err());

        let too_long: Call = "ABCDEFGHI".parse().unwrap();
        let packet = Packet::ConnectViaMarked {
            port: Port(1),
            pid: Pid(0xf0),
            src,
            dst,
            via: vec![ViaHop::seen(too_long)],
        };
        assert!(packet.serialize().is_err());
    }

    #[test]
    fn connection_status_serialization_omits_callsign_padding() {
        let src: Call = "REMOTE".parse().unwrap();
        let dst: Call = "LOCAL".parse().unwrap();
        let bytes = Packet::ConnectionEstablished {
            port: Port(1),
            pid: Pid(0),
            src,
            dst,
        }
        .serialize()
        .unwrap();

        assert_eq!(
            &bytes[crate::HEADER_LEN..],
            b"*** CONNECTED With Station REMOTE"
        );
    }

    #[test]
    fn preserves_unmodeled_packet() {
        let src: Call = "REMOTE".parse().unwrap();
        let dst: Call = "LOCAL".parse().unwrap();
        let header = Header::new(
            Port(1),
            b'U',
            Pid(0xf0),
            Some(src.clone()),
            Some(dst.clone()),
            3,
        );

        assert_eq!(
            Packet::parse(&header, b"UI!").unwrap(),
            Packet::Opaque {
                port: Port(1),
                pid: Pid(0xf0),
                data_kind: b'U',
                src: Some(src),
                dst: Some(dst),
                data: b"UI!".to_vec(),
            }
        );
    }
}

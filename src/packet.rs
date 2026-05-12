use log::{debug, trace};
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
    /// The full AGW payload includes timestamps the crate does not currently
    /// model, so this keeps the raw payload bytes. For an empty heard list,
    /// send a single trailing NUL byte: `vec![0]`.
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
    // FramesOutstandingConnection(u32), // Y
    // HeardStations(String) // H
    // MonitorConnected(Vec<u8>) // I
    // MonitorSupervisory(Vec<u8>) // S
    // Raw() // R.
    // Unknown
}

impl Packet {
    /// Serialize packet for AGW connection.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        match self {
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
            Packet::ConnectVia {
                port,
                pid,
                src,
                dst,
                via,
            } => {
                /*
                const MAX_HOPS: usize = 7;
                if via.len() > MAX_HOPS {
                    return Err(Error::msg(format!(
                    "tried to connect through too many hops: {} > {MAX_HOPS}",
                    via.len()
                    )));
                }
                */
                let mut hops = Vec::new();
                hops.push(u8::try_from(via.len()).expect("TODO: error or something"));
                for call in via {
                    hops.extend_from_slice(call.as_bytes());
                }
                let h = Header::new(
                    *port,
                    CMD_CONNECT_VIA,
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(hops.len()).expect("TODO: error or something"),
                )
                .serialize();
                [h, hops.clone()].concat()
            }
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
            } => {
                let mut chunks = Vec::new();
                trace!("agw: Sending data with pid {pid:?}");
                // TODO: magic number.
                for chunk in data.chunks(200) {
                    chunks.push(
                        Header::new(
                            *port,
                            CMD_DATA,
                            *pid,
                            Some(src.clone()),
                            Some(dst.clone()),
                            u32::try_from(chunk.len())
                                .expect("TODO: error this, or make it impossible"),
                        )
                        .serialize(),
                    );
                    chunks.push(chunk.to_vec());
                }
                chunks
            }
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
        }
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
                        return Err(Error::msg(format!("unknown C {s}")));
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
                for chunk in data[1..].chunks_exact(10) {
                    via.push(Call::from_bytes(chunk)?);
                }
                debug!("agw: Got ConnectVia from {src:?} to {dst:?} via {via:?}");
                Packet::ConnectVia {
                    port: header.port,
                    pid: header.pid,
                    src,
                    dst,
                    via,
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
            _ => {
                return Err(Error::msg(format!(
                    "unknown packet kind {}",
                    header.data_kind
                )));
            }
        })
    }
}

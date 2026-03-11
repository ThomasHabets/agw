use log::debug;

use crate::{Call, Header};
use crate::{Error, Result};

const CMD_CONNECT: u8 = b'C';
const CMD_DATA: u8 = b'D';

/// Port number.
#[derive(Copy, Clone, Debug, PartialEq)]
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

    /// Application: Port capability query.
    PortCapQuery(Port),

    /// Application: Port info query.
    PortInfoQuery,
    RegisterCallsign(Port, Pid, Call),
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
            Packet::VersionQuery => Header::new(Port(0), b'R', Pid(0), None, None, 0).serialize(),
            Packet::FramesOutstandingPortQuery(port) => {
                Header::new(*port, b'y', Pid(0), None, None, 0).serialize()
            }
            Packet::FramesOutstandingPortReply(port, n) => [
                Header::new(*port, b'y', Pid(0), None, None, 4).serialize(),
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
                    Header::new(Port(0), b'R', Pid(0), None, None, 0).serialize(),
                    data,
                ]
                .concat()
            }
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
                Header::new(*port, b'C', *pid, Some(src.clone()), Some(dst.clone()), 0).serialize(),
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
                Header::new(*port, b'C', *pid, Some(src.clone()), Some(dst.clone()), 0).serialize(),
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
                let h = Header::new(*port, b'v', *pid, Some(src.clone()), Some(dst.clone()), 0)
                    .serialize();
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
                [h, hops.clone()].concat()
            }
            Packet::RegisterCallsign(port, pid, src) => {
                Header::new(*port, b'X', *pid, Some(src.clone()), None, 0).serialize()
            }
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
                    b'D',
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(data.len()).expect("TODO: error this, or make it impossible"),
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
                    b'M',
                    *pid,
                    Some(src.clone()),
                    Some(dst.clone()),
                    u32::try_from(data.len()).expect("TODO: return err or something"),
                )
                .serialize(),
                data.clone(),
            ]
            .concat(),
            Packet::PortInfoQuery => Header::new(Port(0), b'G', Pid(0), None, None, 0).serialize(),
            Packet::PortCapQuery(port) => {
                Header::new(*port, b'g', Pid(0), None, None, 0).serialize()
            }
        }
    }
    pub fn parse(header: &Header, data: &[u8]) -> Result<Packet> {
        Ok(match header.data_kind {
            b'R' => {
                if data.len() != 8 {
                    return Err(Error::msg(format!(
                        "version packet had wrong length {}, {data:?}",
                        header.data_kind
                    )));
                }

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
            }
            CMD_CONNECT => {
                let s = String::from_utf8(data.to_vec()).map_err(Error::other)?;
                let src = header
                    .src
                    .clone()
                    .ok_or(Error::msg("connect missing src"))?;
                let dst = header
                    .dst
                    .clone()
                    .ok_or(Error::msg("connect missing src"))?;
                if s.starts_with("*** CONNECTED WITH")
                    || s.starts_with("*** CONNECTED With Station ")
                {
                    debug!("Got ConnectionEstablished {s}");
                    Packet::ConnectionEstablished {
                        port: header.port,
                        pid: header.pid,
                        src: src.clone(),
                        dst: dst.clone(),
                    }
                } else if s.starts_with("*** CONNECTED To Station") {
                    debug!("Got IncomingConnect {s}");
                    Packet::IncomingConnect {
                        port: header.port,
                        pid: header.pid,
                        src: src.clone(),
                        dst: dst.clone(),
                    }
                } else {
                    return Err(Error::msg(format!("unknown C {s}")));
                }
            }
            //b'd' => Packet::Disconnect,
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
            _ => {
                return Err(Error::msg(format!(
                    "unknown packet kind {}",
                    header.data_kind
                )));
            }
        })
    }
}

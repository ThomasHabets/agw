use crate::{Call, Header};
use anyhow::{Error, Result};
use log::debug;

const CMD_CONNECT: u8 = b'C';
const CMD_DATA: u8 = b'D';

#[derive(Debug, PartialEq, Clone)]
pub enum Packet {
    VersionQuery,
    VersionReply(u16, u16),
    PortCap(u8),
    PortInfo(u8),
    RegisterCallsign(u8, u8, Call),
    Connect {
        port: u8,
        pid: u8,
        src: Call,
        dst: Call,
    },
    ConnectVia {
        port: u8,
        pid: u8,
        src: Call,
        dst: Call,
        via: Vec<Call>,
    },
    IncomingConnect {
        port: u8,
        pid: u8,
        src: Call,
        dst: Call,
    },
    ConnectionEstablished {
        port: u8,
        pid: u8,
        src: Call,
        dst: Call,
    },
    Disconnect {
        port: u8,
        pid: u8,
        src: Call,
        dst: Call,
    },
    Unproto {
        port: u8,
        pid: u8,
        src: Call,
        dst: Call,
        data: Vec<u8>,
    },
    Data {
        port: u8,
        pid: u8,
        src: Call,
        dst: Call,
        data: Vec<u8>,
    },
    // FramesOutstandingPort(u32), // y
    // FramesOutstandingConnection(u32), // Y
    // HeardStations(String) // H
    // MonitorConnected(Vec<u8>) // I
    // MonitorSupervisory(Vec<u8>) // S
    // Raw() // R.
    // Unknown
}

impl Packet {
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            Packet::VersionQuery => Header::new(0, b'R', 0, None, None, 0).serialize(),
            Packet::VersionReply(maj, min) => {
                let data = vec![
                    *maj as u8,
                    (*maj >> 8) as u8,
                    0,
                    0,
                    *min as u8,
                    (*min >> 8) as u8,
                    0,
                    0,
                ];
                [Header::new(0, b'R', 0, None, None, 0).serialize(), data].concat()
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
                format!("*** CONNECTED To Station {}", src.string())
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
                format!("*** CONNECTED With Station {}", src.string())
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
                hops.push(via.len() as u8);
                for call in via {
                    hops.extend_from_slice(call.bytes());
                }
                [h, hops.to_vec()].concat()
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
                    data.len() as u32,
                )
                .serialize(),
                data.to_vec(),
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
                    data.len() as u32,
                )
                .serialize(),
                data.to_vec(),
            ]
            .concat(),
            Packet::PortInfo(port) => Header::new(*port, b'G', 0, None, None, 0).serialize(),
            Packet::PortCap(port) => Header::new(*port, b'g', 0, None, None, 0).serialize(),
        }
    }
    pub fn parse(header: &Header, data: &[u8]) -> Result<Packet> {
        Ok(match header.data_kind() {
            b'R' => {
                if data.len() != 8 {
                    return Err(Error::msg(format!(
                        "version packet had wrong length {}, {data:?}",
                        header.data_kind()
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
                Packet::VersionReply(major, minor)
            }
            CMD_CONNECT => {
                let s = String::from_utf8(data.to_vec())?;
                let src = header
                    .src()
                    .clone()
                    .ok_or(Error::msg("connect missing src"))?;
                let dst = header
                    .dst()
                    .clone()
                    .ok_or(Error::msg("connect missing src"))?;
                if s.starts_with("*** CONNECTED WITH")
                    || s.starts_with("*** CONNECTED With Station ")
                {
                    debug!("Got ConnectionEstablished {s}");
                    Packet::ConnectionEstablished {
                        port: header.port(),
                        pid: header.pid(),
                        src: src.clone(),
                        dst: dst.clone(),
                    }
                } else if s.starts_with("*** CONNECTED To Station") {
                    debug!("Got IncomingConnect {s}");
                    Packet::IncomingConnect {
                        port: header.port(),
                        pid: header.pid(),
                        src: src.clone(),
                        dst: dst.clone(),
                    }
                } else {
                    return Err(Error::msg(format!("unknown C {s}")));
                }
            }
            //b'd' => Packet::Disconnect,
            CMD_DATA => Packet::Data {
                port: header.port(),
                pid: header.pid(),
                src: header
                    .src()
                    .clone()
                    .ok_or(Error::msg("data with missing src"))?,
                dst: header
                    .dst()
                    .clone()
                    .ok_or(Error::msg("data with missing dst"))?,
                data: data.to_vec(),
            },
            _ => {
                return Err(Error::msg(format!(
                    "unknown packet kind {}",
                    header.data_kind()
                )));
            }
        })
    }
}

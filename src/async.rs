use anyhow::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{parse_header, Call, Packet, HEADER_LEN};

pub struct AGW {
    con: TcpStream,
}

impl AGW {
    pub async fn new(addr: &str) -> Result<AGW> {
        Ok(Self {
            con: TcpStream::connect(addr).await?,
        })
    }
    pub async fn send_raw(&mut self, msg: &[u8]) -> Result<(), std::io::Error> {
        self.con.write_all(msg).await
    }
    pub async fn send(&mut self, data: Packet) -> Result<(), std::io::Error> {
        self.send_raw(&data.serialize()).await
    }
    pub async fn recv(&mut self) -> Result<Packet> {
        let mut header = [0_u8; HEADER_LEN];
        self.con.read_exact(&mut header).await?;
        let header = parse_header(&header)?;
        let payload = if header.data_len() > 0 {
            let mut payload = vec![0; header.data_len() as usize];
            self.con.read_exact(&mut payload).await?;
            payload
        } else {
            Vec::new()
        };
        Packet::parse(&header, &payload)
    }
    pub async fn connect<'a>(
        &'a mut self,
        port: u8,
        pid: u8,
        src: &Call,
        dst: &Call,
        _via: &[Call],
    ) -> Result<Connection<'a>> {
        // TODO: actually start a connection.
        Ok(Connection {
            connect_string: "TODO".to_string(),
            port,
            pid,
            src: src.clone(),
            dst: dst.clone(),
            agw: self,
            disconnected: false,
        })
    }
}

/// AX.25 connection object.
///
/// Created from an AGW object, using `.connect()`.
pub struct Connection<'a> {
    connect_string: String,
    port: u8,
    pid: u8,
    src: Call,
    dst: Call,
    agw: &'a mut AGW,
    disconnected: bool,
}

impl<'a> Connection<'a> {
    pub async fn recv(&mut self) -> Result<Packet> {
        let _ = self.connect_string;
        let _ = self.disconnected;
        let _ = self.src;
        let _ = self.dst;
        todo!()
    }
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let packet = Packet::Data {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
            data: data.to_vec(),
        };
        self.agw
            .send(packet)
            .await
            .map_err(|e| Error::msg(format!("{e:?}")))
    }
}

use anyhow::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::{parse_header, Call, Packet, HEADER_LEN};

// TODO: ideally this should self-unregister when ident goes out of
// scope.
type RuleIdent = u64;

#[derive(Clone)]
pub enum RuleMatch {
    Data(u8),
    ConnectionEstablished { port: u8, src: Call, dst: Call },
}

#[derive(Clone)]
pub struct Rule {
    ident: RuleIdent,
    m: RuleMatch,
    tx: mpsc::Sender<Packet>,
}

impl RuleMatch {
    fn matches(&self, packet: &Packet) -> bool {
        match self {
            RuleMatch::Data(port) => {
                if let Packet::Data {
                    port: port2,
                    pid: _,
                    src: _,
                    dst: _,
                    data: _,
                } = packet
                {
                    return port == port2;
                }
            }
            RuleMatch::ConnectionEstablished { port, src, dst } => {
                if let Packet::ConnectionEstablished {
                    port: port2,
                    pid: _,
                    src: src2,
                    dst: dst2,
                } = packet
                {
                    return port == port2 && src == src2 && dst == dst2;
                }
            }
        };
        false
    }
}

pub struct Router {
    ident: RuleIdent,
    rules: Vec<Rule>,
}

impl Router {
    pub fn new() -> Router {
        Self {
            ident: 0,
            rules: Vec::new(),
        }
    }
    pub fn add(&mut self, m: RuleMatch, tx: mpsc::Sender<Packet>) -> RuleIdent {
        self.ident += 1;
        self.rules.push(Rule {
            m,
            ident: self.ident,
            tx,
        });
        self.ident
    }
    pub fn del(&mut self, ident: RuleIdent) {
        // TODO: there has to be a more efficient way.
        //
        // Well, obviously once the rule ident is higher than the
        // `ident`, it will no longer match. Or when it's already
        // matched.
        self.rules = self
            .rules
            .iter()
            .filter(|&r| r.ident != ident)
            .map(|r| r.to_owned())
            .collect();
    }
    pub async fn process(&mut self, packet: Packet) -> Result<bool> {
        let mut any = false;
        for rule in self.rules.iter() {
            if rule.m.matches(&packet) {
                rule.tx.send(packet.clone()).await?;
                any = true;
            }
        }
        Ok(any)
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AGW {
    con: TcpStream,
    router: Router,
}

impl AGW {
    pub async fn new(addr: &str) -> Result<AGW> {
        Ok(Self {
            con: TcpStream::connect(addr).await?,
            router: Router::new(),
        })
    }
    pub async fn send_raw(&mut self, msg: &[u8]) -> Result<(), std::io::Error> {
        self.con.write_all(msg).await
    }
    pub async fn send(&mut self, data: Packet) -> Result<(), std::io::Error> {
        self.send_raw(&data.serialize()).await
    }
    pub async fn route(&mut self) -> Result<()> {
        loop {
            let packet = self.read_packet().await?;
            self.router.process(packet).await?;
        }
    }
    pub async fn read_packet(&mut self) -> Result<Packet> {
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
        let (tx, mut rx) = mpsc::channel(1);
        let ident = self.router.add(
            RuleMatch::ConnectionEstablished {
                port,
                src: dst.clone(),
                dst: src.clone(),
            },
            tx,
        );
        self.send(Packet::Connect {
            port,
            pid,
            src: src.clone(),
            dst: dst.clone(),
        })
        .await?;
        let estab = rx.recv().await.ok_or(Error::msg("TODO"))?;
        self.router.del(ident);
        match estab {
            Packet::ConnectionEstablished {
                port: _,
                pid: _,
                src: _,
                dst: _,
            } => Ok(Connection {
                connect_string: "TODO".to_string(),
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
                agw: self,
                disconnected: false,
            }),
            other => {
                panic!("received unexpected packet: {other:?}")
            }
        }
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

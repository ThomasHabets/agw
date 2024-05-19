use anyhow::{Error, Result};
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::{parse_header, Call, Packet, HEADER_LEN};

type RuleIdent = u64;

pub struct RuleHandle {
    ident: RuleIdent,
    router: Weak<Router>,
}

impl RuleHandle {
    fn new(ident: RuleIdent, router: Weak<Router>) -> Self {
        Self { ident, router }
    }
}

impl Drop for RuleHandle {
    fn drop(&mut self) {
        if let Some(router) = self.router.upgrade() {
            router.del(self.ident);
        }
    }
}

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
    ident: Mutex<RuleIdent>,
    rules: Arc<Mutex<Vec<Rule>>>,
}

impl Router {
    pub fn new() -> Router {
        Self {
            ident: Mutex::new(0),
            rules: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn add(self: &Arc<Self>, m: RuleMatch, tx: mpsc::Sender<Packet>) -> RuleHandle {
        let ident = {
            let mut ident = self.ident.lock().unwrap();
            *ident += 1;
            *ident
        };
        self.rules.lock().unwrap().push(Rule { m, ident, tx });
        RuleHandle::new(ident, Arc::downgrade(self))
    }
    pub fn del(&self, ident: RuleIdent) {
        // TODO: there has to be a more efficient way.
        //
        // Well, obviously once the rule ident is higher than the
        // `ident`, it will no longer match. Or when it's already
        // matched.
        let mut rules = self.rules.lock().unwrap();
        *rules = rules
            .iter()
            .filter(|&r| r.ident != ident)
            .map(|r| r.to_owned())
            .collect();
    }
    pub async fn process(&self, packet: Packet) -> Result<bool> {
        let mut any = false;
        let rules = self.rules.lock().unwrap().clone();
        for rule in rules.iter() {
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
    router: Arc<Router>,
}

impl AGW {
    pub async fn new(addr: &str) -> Result<AGW> {
        Ok(Self {
            con: TcpStream::connect(addr).await?,
            router: Arc::new(Router::new()),
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
        if let Err(e) = self
            .send(Packet::Connect {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
            })
            .await
        {
            return Err(Error::msg(format!("{e:?}")));
        }
        let estab = rx.recv().await.ok_or(Error::msg("TODO"));
        drop(ident);
        let estab = estab?;
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

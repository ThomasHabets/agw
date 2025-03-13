use anyhow::{Error, Result};
use log::debug;
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::{parse_header, Call, Header, Packet, HEADER_LEN};

const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
    Data { port: u8, src: Call, dst: Call },
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
            RuleMatch::Data { port, src, dst } => {
                if let Packet::Data {
                    port: port2,
                    pid: _,
                    src: src2,
                    dst: dst2,
                    data: _,
                } = packet
                {
                    return port == port2 && src == src2 && dst == dst2;
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
        // TODO: not very efficient, but it avoids holding the lock
        // cross await.
        let rules = self.rules.lock().unwrap().clone();
        for rule in rules.iter() {
            if rule.m.matches(&packet) {
                rule.tx.send(packet.clone()).await?;
                any = true;
            }
        }
        if !any {
            debug!("incoming packet had no match: {packet:?}");
        }
        Ok(any)
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// Packet in, packet out.
struct Pipo {
    tx: mpsc::Sender<Packet>,
    //rx: tokio::sync::Mutex<mpsc::Receiver<Packet>>,
}

enum PIPOState {
    AwaitHeader,
    GotHeader(Header),
}

impl Pipo {
    async fn new(con: TcpStream, router: Arc<Router>) -> Self {
        //let (tx1, rx1) = mpsc::channel(10); // TODO: magic number.
        let (tx2, rx2) = mpsc::channel(10); // TODO: magic number.
        tokio::spawn(async move {
            Self::run(con, router, rx2)
                .await
                .expect("Pipo run() failed");
        });
        Pipo {
            tx: tx2,
            //rx: tokio::sync::Mutex::new(rx1),
        }
    }
    async fn send(&self, packet: Packet) -> Result<()> {
        self.tx.send(packet).await.map_err(|e| anyhow::anyhow!(e))
    }
    /*    async fn recv(&self) -> Option<Packet> {
        self.rx.lock().await.recv().await
    } */
    async fn run(
        mut con: TcpStream,
        router: Arc<Router>,
        mut rx: mpsc::Receiver<Packet>,
    ) -> Result<()> {
        let mut state = PIPOState::AwaitHeader;
        loop {
            match state {
                PIPOState::AwaitHeader => {
                    let mut header = [0_u8; HEADER_LEN];
                    tokio::select! {
                    // TODO: what happens to partial reads?
                    ok = con.read_exact(&mut header) => {
                        ok?;
                        state = PIPOState::GotHeader(parse_header(&header)?)
                    },
                    p = rx.recv() => match p {
                        Some(p) => con.write_all(&p.serialize()).await?,
                        // TODO: continue reading even while write
                        // blocks.
                        None => return Ok(()),
                    },
                    };
                }
                PIPOState::GotHeader(ref header) => {
                    if header.data_len() > 0 {
                        let mut payload = vec![0; header.data_len() as usize];
                        tokio::select! {
                                        ok = con.read_exact(&mut payload) => {
                            ok?;
                        let packet = Packet::parse(header, &payload)?;
                        debug!("Sending off packet {packet:?}");
                            router.process(packet).await?;
                        debug!("packet sent");
                            state = PIPOState::AwaitHeader;
                                        },
                                        p = rx.recv() => match p {
                            Some(p) => con.write_all(&p.serialize()).await?,
                            // TODO: should we continue receiving
                            // from con, still?
                            None => return Ok(()),
                                        },
                                    };
                    }
                }
            };
        }
    }
}

pub struct AGW {
    con: Pipo,
    router: Arc<Router>,
}

impl AGW {
    pub async fn new(addr: &str) -> Result<AGW> {
        let router = Arc::new(Router::new());
        let r2 = router.clone();
        Ok(Self {
            con: Pipo::new(TcpStream::connect(addr).await?, r2).await,
            router,
        })
    }
    pub async fn send(&self, data: Packet) -> Result<()> {
        self.con.send(data).await
    }
    /*
        pub async fn recv(&self) -> Option<Packet> {
            self.con.recv().await
    }
        */
    pub async fn connect<'a>(
        &'a self,
        port: u8,
        pid: u8,
        src: &Call,
        dst: &Call,
        _via: &[Call],
    ) -> Result<Connection<'a>> {
        let (tx, mut rx) = mpsc::channel(1);

        // Register rule for receiving connection established.
        let ident = self.router.add(
            RuleMatch::ConnectionEstablished {
                port,
                src: dst.clone(),
                dst: src.clone(),
            },
            tx,
        );

        // Also register to receive data.
        let (txd, rxd) = mpsc::channel(10); // TODO: magic number.
        let rule_handle = self.router.add(
            RuleMatch::Data {
                port,
                src: dst.clone(),
                dst: src.clone(),
            },
            txd,
        );

        // Send connection establish.
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

        // Wait for connection established.
        let estab = tokio::time::timeout(CONNECTION_TIMEOUT, rx.recv())
            .await
            .expect("connection timeout")
            .ok_or(Error::msg("TODO"));
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
                _rule_handle: rule_handle,
                rx: rxd,
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
    agw: &'a AGW,
    disconnected: bool,
    _rule_handle: RuleHandle,
    rx: mpsc::Receiver<Packet>,
}

impl Connection<'_> {
    pub async fn recv(&mut self) -> Result<Packet> {
        let _ = self.connect_string;
        let _ = self.disconnected;
        let _ = self.src;
        let _ = self.dst;
        self.rx.recv().await.ok_or(Error::msg("recv failed"))
    }
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let packet = Packet::Data {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
            data: data.to_vec(),
        };
        self.agw.send(packet).await
    }
}

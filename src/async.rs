use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use log::{debug, trace};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::{parse_header, Call, Header, Packet, Pid, Port, HEADER_LEN};
use crate::{Error, Result};

const PID_AX25: Pid = Pid(0xf0);
const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);

type RuleIdent = u64;

pub struct RuleHandle {
    ident: RuleIdent,
    rules: Weak<Mutex<Vec<Rule>>>,
}

impl RuleHandle {
    fn new(ident: RuleIdent, rules: Weak<Mutex<Vec<Rule>>>) -> Self {
        Self { ident, rules }
    }
}

impl Drop for RuleHandle {
    fn drop(&mut self) {
        if let Some(rules) = self.rules.upgrade() {
            let mut rules = rules.lock().unwrap();
            *rules = rules
                .iter()
                .filter(|&r| r.ident != self.ident)
                .map(std::borrow::ToOwned::to_owned)
                .collect();
        }
    }
}

#[derive(Clone)]
pub enum RuleMatch {
    Data { port: Port, src: Call, dst: Call },
    ConnectionEstablished { port: Port, src: Call, dst: Call },
    IncomingConnect { port: Port, dst: Call },
}

#[derive(Clone)]
pub struct Rule {
    ident: RuleIdent,
    m: RuleMatch,
    sink: RuleSink,
}

#[derive(Clone)]
enum RuleSink {
    Packet(mpsc::Sender<Packet>),
    Listener(mpsc::Sender<PendingConnection>),
}

impl RuleMatch {
    fn matches(&self, packet: &Packet) -> bool {
        match self {
            RuleMatch::Data { port, src, dst } => match packet {
                Packet::Data {
                    port: port2,
                    pid: _,
                    src: src2,
                    dst: dst2,
                    data: _,
                }
                | Packet::Disconnect {
                    port: port2,
                    pid: _,
                    src: src2,
                    dst: dst2,
                } => {
                    return port == port2 && src == src2 && dst == dst2;
                }
                _ => {}
            },
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
            RuleMatch::IncomingConnect { port, dst } => {
                if let Packet::IncomingConnect {
                    port: port2,
                    pid: _,
                    src: _,
                    dst: dst2,
                } = packet
                {
                    return port == port2 && dst == dst2;
                }
            }
        }
        false
    }
}

pub struct Router {
    ident: Mutex<RuleIdent>,
    rules: Arc<Mutex<Vec<Rule>>>,
}

impl Router {
    #[must_use]
    pub fn new() -> Router {
        Self {
            ident: Mutex::new(0),
            rules: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn add(self: &Arc<Self>, m: RuleMatch, tx: mpsc::Sender<Packet>) -> RuleHandle {
        self.add_inner(m, RuleSink::Packet(tx))
    }
    fn add_incoming_listener(
        &self,
        port: Port,
        dst: Call,
        tx: mpsc::Sender<PendingConnection>,
    ) -> RuleHandle {
        self.add_inner(
            RuleMatch::IncomingConnect { port, dst },
            RuleSink::Listener(tx),
        )
    }
    fn add_inner(&self, m: RuleMatch, sink: RuleSink) -> RuleHandle {
        let ident = {
            let mut ident = self.ident.lock().unwrap();
            *ident += 1;
            *ident
        };
        self.rules.lock().unwrap().push(Rule { ident, m, sink });
        RuleHandle::new(ident, Arc::downgrade(&self.rules))
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
            .map(std::borrow::ToOwned::to_owned)
            .collect();
    }
    pub async fn process(&self, packet: Packet) -> Result<bool> {
        let mut any = false;
        // TODO: not very efficient, but it avoids holding the lock
        // cross await.
        let rules = self.rules.lock().unwrap().clone();
        for rule in &rules {
            if rule.m.matches(&packet) {
                match &rule.sink {
                    RuleSink::Packet(tx) => {
                        tx.send(packet.clone()).await.map_err(Error::other)?;
                    }
                    RuleSink::Listener(tx) => {
                        let Packet::IncomingConnect {
                            port,
                            pid: _,
                            src,
                            dst,
                        } = &packet
                        else {
                            continue;
                        };
                        let (txd, rxd) = mpsc::channel(10); // TODO: magic number.
                        let rule_handle = self.add_inner(
                            RuleMatch::Data {
                                port: *port,
                                src: src.clone(),
                                dst: dst.clone(),
                            },
                            RuleSink::Packet(txd),
                        );
                        tx.send(PendingConnection {
                            port: *port,
                            pid: PID_AX25, // IncomingConnect always has pid 0x00.
                            src: dst.clone(),
                            dst: src.clone(),
                            rule_handle,
                            rx: rxd,
                        })
                        .await
                        .map_err(Error::other)?;
                    }
                }
                any = true;
            }
        }
        if !any {
            debug!("agw: incoming packet had no match: {packet:?}");
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
    fn new(con: TcpStream, router: Arc<Router>) -> Self {
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
        self.tx.send(packet).await.map_err(Error::other)
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
                        state = PIPOState::GotHeader(parse_header(&header)?);
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
                    if header.data_len > 0 {
                        let mut payload = vec![0; header.data_len as usize];
                        tokio::select! {
                            ok = con.read_exact(&mut payload) => {
                                ok?;
                                let packet = Packet::parse(header, &payload)?;
                                debug!("agw/pipo: Processing {packet:?}");
                                router.process(packet).await?;
                                state = PIPOState::AwaitHeader;
                            },
                            p = rx.recv() => match p {
                                Some(p) => con.write_all(&p.serialize()).await?,
                                // TODO: should we continue receiving
                                // from con, still? Could deadlock?
                                None => return Ok(()),
                            },
                        };
                    } else {
                        // Disconnect.
                        let packet = Packet::parse(header, &[])?;
                        debug!("agw/pipo: Processing (should be Disconnect) {packet:?}");
                        router.process(packet).await?;
                        state = PIPOState::AwaitHeader;
                    }
                }
            }
        }
    }
}

pub struct AGW {
    con: Pipo,
    router: Arc<Router>,
}

impl AGW {
    /// Connect to AGWPE.
    ///
    /// # Errors
    ///
    /// If connection establishment fails.
    pub async fn new(addr: &str) -> Result<AGW> {
        let router = Arc::new(Router::new());
        let r2 = router.clone();
        Ok(Self {
            con: Pipo::new(TcpStream::connect(addr).await?, r2),
            router,
        })
    }
    /// Send some data on connection.
    ///
    /// # Errors
    ///
    /// Errors if the underlying connection fails.
    pub async fn send(&self, data: Packet) -> Result<()> {
        self.con.send(data).await
    }

    /// Register callsign.
    ///
    /// The specs say that registering the callsign is mandatory.
    /// Direwolf doesn't seem to care, but other AGW implementations may.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub async fn register_callsign(&self, port: Port, src: &Call) -> Result<()> {
        self.send(Packet::RegisterCallsign(port, src.clone())).await
    }

    /// Listen for incoming connections to a local callsign.
    ///
    /// This registers the callsign with the AGW endpoint and then returns
    /// a listener that can `accept()` incoming AX.25 connections.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub async fn listen<'a>(&'a self, port: Port, src: &Call) -> Result<Listener<'a>> {
        let (tx, rx) = mpsc::channel(10); // TODO: magic number.
        let rule_handle = self.router.add_incoming_listener(port, src.clone(), tx);
        self.register_callsign(port, src).await?;
        Ok(Listener {
            agw: self,
            _rule_handle: rule_handle,
            rx,
        })
    }

    fn make_connection(
        &self,
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
        rule_handle: RuleHandle,
        rx: mpsc::Receiver<Packet>,
    ) -> Connection<'_> {
        Connection {
            connect_string: "TODO".to_string(),
            port,
            pid,
            src,
            dst,
            agw: self,
            _rule_handle: rule_handle,
            rx,
            read_buf: vec![],
            pending_write: None,
            pending_shutdown: None,
            disconnected: false,
        }
    }

    /// # Errors
    ///
    /// If the underlying connection fails.
    pub async fn connect<'a>(
        &'a self,
        port: Port,
        pid: Pid,
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
            .map_err(Error::other)?
            .ok_or(Error::msg("no packet"));
        drop(ident);

        let estab = estab?;
        trace!("agw: Awaiting connection establishment packet having pid {pid:?}");
        match estab {
            Packet::ConnectionEstablished {
                port: _,
                pid: _,
                src: _,
                dst: _,
            } => {
                trace!("agw: Connection established!");
                Ok(self.make_connection(port, pid, src.clone(), dst.clone(), rule_handle, rxd))
            }
            other => {
                panic!("received unexpected packet: {other:?}")
            }
        }
    }
}

/// Listener for incoming AX.25 connections.
///
/// Created from an AGW object, using `.listen()`.
pub struct Listener<'a> {
    agw: &'a AGW,
    _rule_handle: RuleHandle,
    rx: mpsc::Receiver<PendingConnection>,
}

impl<'a> Listener<'a> {
    /// Accept an incoming connection.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub async fn accept(&mut self) -> Result<Connection<'a>> {
        let pending = self.rx.recv().await.ok_or(Error::msg("recv failed"))?;
        Ok(self.agw.make_connection(
            pending.port,
            pending.pid,
            pending.src,
            pending.dst,
            pending.rule_handle,
            pending.rx,
        ))
    }
}

struct PendingConnection {
    port: Port,
    pid: Pid,
    src: Call,
    dst: Call,
    rule_handle: RuleHandle,
    rx: mpsc::Receiver<Packet>,
}

/// AX.25 connection object.
///
/// Created from an AGW object, using `.connect()`.
pub struct Connection<'a> {
    connect_string: String,
    port: Port,
    pid: Pid,
    src: Call,
    dst: Call,
    agw: &'a AGW,
    disconnected: bool,
    _rule_handle: RuleHandle,
    rx: mpsc::Receiver<Packet>,
    read_buf: Vec<u8>,
    pending_write: Option<PendingWrite>,
    pending_shutdown: Option<PendingSend>,
}

impl Connection<'_> {
    /// Return the local callsign.
    #[must_use]
    pub fn src(&self) -> &Call {
        &self.src
    }

    /// Return the remote callsign.
    #[must_use]
    pub fn dst(&self) -> &Call {
        &self.dst
    }

    /// Return the AGW port number.
    #[must_use]
    pub fn port(&self) -> Port {
        self.port
    }

    /// Return the PID used by this connection.
    #[must_use]
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Receive packet from connection.
    ///
    /// # Errors
    ///
    /// Fails if the connection fails.
    pub async fn recv(&mut self) -> Result<Packet> {
        let _ = &self.connect_string;
        self.rx.recv().await.ok_or(Error::msg("recv failed"))
    }
    /// Send data on connection.
    ///
    /// # Errors
    ///
    /// Fails if the connection fails.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        if self.disconnected {
            return Err(Error::msg("connection disconnected"));
        }
        let packet = self.data_packet(data.to_vec());
        self.agw.send(packet).await
    }

    fn data_packet(&self, data: Vec<u8>) -> Packet {
        Packet::Data {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
            data,
        }
    }

    fn disconnect_packet(&self) -> Packet {
        Packet::Disconnect {
            port: self.port,
            pid: self.pid,
            src: self.src.clone(),
            dst: self.dst.clone(),
        }
    }

    fn send_future(&self, packet: Packet) -> PendingSend {
        let tx = self.agw.con.tx.clone();
        Box::pin(async move { tx.send(packet).await })
    }

    fn drain_read_buf(&mut self, buf: &mut ReadBuf<'_>) {
        let n = buf.remaining().min(self.read_buf.len());
        buf.put_slice(&self.read_buf[..n]);
        self.read_buf.drain(..n);
    }

    fn poll_pending_write(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<usize>> {
        let Some(pending) = self.pending_write.as_mut() else {
            return Poll::Ready(Ok(0));
        };
        match pending.fut.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => {
                let len = pending.len;
                self.pending_write = None;
                Poll::Ready(Ok(len))
            }
            Poll::Ready(Err(e)) => {
                self.pending_write = None;
                self.disconnected = true;
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e.to_string(),
                )))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_pending_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let Some(pending) = self.pending_shutdown.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match pending.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => {
                self.pending_shutdown = None;
                self.disconnected = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                self.pending_shutdown = None;
                self.disconnected = true;
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e.to_string(),
                )))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

type PendingSend =
    Pin<Box<dyn Future<Output = std::result::Result<(), mpsc::error::SendError<Packet>>> + Send>>;

struct PendingWrite {
    len: usize,
    fut: PendingSend,
}

impl AsyncRead for Connection<'_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if !this.read_buf.is_empty() {
            this.drain_read_buf(buf);
            return Poll::Ready(Ok(()));
        }
        if this.disconnected {
            return Poll::Ready(Ok(()));
        }
        loop {
            match Pin::new(&mut this.rx).poll_recv(cx) {
                Poll::Ready(Some(Packet::Data { data, .. })) => {
                    if data.is_empty() {
                        continue;
                    }
                    this.read_buf.extend(data);
                    this.drain_read_buf(buf);
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Packet::Disconnect { .. })) => {
                    debug!("agw: Disconnect frame");
                    this.disconnected = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(other)) => {
                    debug!("agw: Ignoring non-data packet on connection stream: {other:?}");
                }
                Poll::Ready(None) => {
                    debug!("agw: EOF");
                    this.disconnected = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for Connection<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match this.poll_pending_shutdown(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }
        if this.pending_shutdown.is_none() && this.disconnected {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "connection disconnected",
            )));
        }
        match this.poll_pending_write(cx) {
            Poll::Ready(Ok(n)) if n > 0 => return Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(_)) => {}
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        this.pending_write = Some(PendingWrite {
            len: buf.len(),
            fut: this.send_future(this.data_packet(buf.to_vec())),
        });
        this.poll_pending_write(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.poll_pending_write(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.poll_pending_write(cx) {
            Poll::Ready(Ok(_)) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }
        if this.disconnected && this.pending_shutdown.is_none() {
            return Poll::Ready(Ok(()));
        }
        if this.pending_shutdown.is_none() {
            this.pending_shutdown = Some(this.send_future(this.disconnect_packet()));
        }
        this.poll_pending_shutdown(cx)
    }
}

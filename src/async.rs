// TODO: This code was a bit hastily written. With the benefit of hindsight it
// should be restructured with a more though through design.
use std::collections::HashMap;
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

/// 3-tuple for a connection.
///
/// We treat a duplicate `IncomingConnect` for the same `(port, local, remote)`
/// as "the UA was lost, replay server-side startup data", not as a new
/// connection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ServerConnectionKey {
    port: Port,
    local: Call,
    remote: Call,
}

/// We initially need some state for a connection in case the UA gets lost and
/// we need to re-send it.
///
/// This is shared between the router and the accepted `Connection`: the router
/// needs to find and replay buffered packets when a duplicate
/// `IncomingConnect` arrives, while the `Connection` needs to append sent
/// packets and mark the connection confirmed after the first client data
/// frame.
#[derive(Default)]
struct ServerConnectionState {
    confirmed: bool,
    buffered: Vec<Packet>,
}

// This has to be an `Arc` because the router and the accepted connection live
// independently, but both need to mutate the same confirmation / replay state.
type SharedServerConnectionState = Arc<Mutex<ServerConnectionState>>;

#[derive(Clone)]
pub struct Rule {
    ident: RuleIdent,
    m: RuleMatch,
    sink: RuleSink,
}

#[derive(Clone)]
enum RuleSink {
    Packet {
        tx: mpsc::Sender<Packet>,
        server_state: Option<SharedServerConnectionState>,
    },
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

/// All packets from Pipo go to the router, which has "rules" about which
/// packets will go where.
pub struct Router {
    ident: Mutex<RuleIdent>,
    // This needs cleaning up. It only allows one AGW "upstream". Do we want
    // that?
    outgoing: Mutex<Option<mpsc::Sender<Packet>>>,
    // Track accepted inbound connections by AX.25 tuple so a duplicate SABM /
    // `IncomingConnect` can find the existing connection and trigger a replay
    // of any server data sent before the client proved it received the UA.
    server_connections: Mutex<HashMap<ServerConnectionKey, Weak<Mutex<ServerConnectionState>>>>,
    rules: Arc<Mutex<Vec<Rule>>>,
}

impl Router {
    #[must_use]
    pub fn new() -> Router {
        Self {
            ident: Mutex::new(0),
            outgoing: Mutex::new(None),
            server_connections: Mutex::new(HashMap::new()),
            rules: Arc::new(Mutex::new(Vec::new())),
        }
    }
    /// Add packet listener. When a packet matches the rules, send it on the
    /// mpsc.
    pub fn add(self: &Arc<Self>, m: RuleMatch, tx: mpsc::Sender<Packet>) -> RuleHandle {
        self.add_inner(
            m,
            RuleSink::Packet {
                tx,
                server_state: None,
            },
        )
    }
    fn add_server_connection(
        &self,
        key: &ServerConnectionKey,
        src: Call,
        dst: Call,
        tx: mpsc::Sender<Packet>,
        server_state: SharedServerConnectionState,
    ) -> RuleHandle {
        // Store only a `Weak` here so dropping the accepted connection is
        // enough to let this bookkeeping disappear without an explicit cleanup
        // path.
        self.server_connections
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::downgrade(&server_state));
        self.add_inner(
            RuleMatch::Data {
                port: key.port,
                src,
                dst,
            },
            RuleSink::Packet {
                tx,
                server_state: Some(server_state),
            },
        )
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
    fn set_outgoing(&self, tx: mpsc::Sender<Packet>) -> Result<()> {
        let mut guard = self.outgoing.lock().unwrap();
        if guard.is_some() {
            return Err(Error::msg(
                "outgoing path already set. Tried to set a second time",
            ));
        }
        *guard = Some(tx);
        Ok(())
    }
    fn outgoing(&self) -> Result<mpsc::Sender<Packet>> {
        self.outgoing
            .lock()
            .unwrap()
            .clone()
            .ok_or(Error::msg("router outgoing sender not configured"))
    }
    fn get_server_connection(
        &self,
        key: &ServerConnectionKey,
    ) -> Option<SharedServerConnectionState> {
        let mut server_connections = self.server_connections.lock().unwrap();
        if let Some(state) = server_connections.get(key).and_then(Weak::upgrade) {
            Some(state)
        } else {
            server_connections.remove(key);
            None
        }
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
                    RuleSink::Packet { tx, server_state } => {
                        if matches!(packet, Packet::Data { .. }) {
                            if let Some(server_state) = server_state {
                                let mut server_state = server_state.lock().unwrap();
                                server_state.confirmed = true;
                                server_state.buffered.clear();
                            }
                        }
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
                        let key = ServerConnectionKey {
                            port: *port,
                            local: dst.clone(),
                            remote: src.clone(),
                        };
                        if let Some(server_state) = self.get_server_connection(&key) {
                            // This is a retransmitted SABM for an already
                            // accepted inbound connection. Until we see any
                            // client data, the client may not have received
                            // our UA, so replay the server's early data.
                            let buffered = {
                                let server_state = server_state.lock().unwrap();
                                if server_state.confirmed {
                                    Vec::new()
                                } else {
                                    server_state.buffered.clone()
                                }
                            };
                            if !buffered.is_empty() {
                                let outgoing = self.outgoing()?;
                                for packet in buffered {
                                    outgoing.send(packet).await.map_err(Error::other)?;
                                }
                            }
                            any = true;
                            continue;
                        }
                        let (txd, rxd) = mpsc::channel(10); // TODO: magic number.
                        let server_state = Arc::new(Mutex::new(ServerConnectionState::default()));
                        let rule_handle = self.add_server_connection(
                            &key,
                            src.clone(),
                            dst.clone(),
                            txd,
                            server_state.clone(),
                        );
                        tx.send(PendingConnection {
                            port: *port,
                            pid: PID_AX25, // IncomingConnect always has pid 0x00.
                            src: dst.clone(),
                            dst: src.clone(),
                            rule_handle,
                            rx: rxd,
                            server_state,
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
///
/// This is the gateway between the AGW TCP stream and the `Router`.
struct Pipo {
    tx: mpsc::Sender<Packet>,
    //rx: tokio::sync::Mutex<mpsc::Receiver<Packet>>,
}

enum PIPOState {
    AwaitHeader,
    GotHeader(Header),
}

impl Pipo {
    fn new(con: TcpStream, router: Arc<Router>) -> Result<Self> {
        //let (tx1, rx1) = mpsc::channel(10); // TODO: magic number.
        let (tx2, rx2) = mpsc::channel(10); // TODO: magic number.
        router.set_outgoing(tx2.clone())?;

        // TODO: probably should split this task in two.
        tokio::spawn(async move {
            Self::run(con, router, rx2)
                .await
                .expect("Pipo run() failed");
        });
        Ok(Pipo {
            tx: tx2,
            //rx: tokio::sync::Mutex::new(rx1),
        })
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
                                debug!("agw/pipo: Processing packet len {}", header.data_len);
                                trace!("agw/pipo: Processing packet {packet:?}");
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

/// Packet-oriented AGW server-side connection wrapper.
///
/// This is intended for code that is implementing an AGW server rather than
/// talking to one. It does not spawn background tasks or route packets to
/// per-connection objects; it simply reads and writes `Packet` values on a
/// single accepted TCP stream.
pub struct AGWServer {
    con: TcpStream,
}

impl AGWServer {
    /// Wrap an accepted AGW TCP connection.
    #[must_use]
    pub fn new(con: TcpStream) -> Self {
        Self { con }
    }

    /// Return the peer socket address.
    ///
    /// # Errors
    ///
    /// If the underlying socket query fails.
    pub fn peer_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.con.peer_addr()
    }

    /// Return the local socket address.
    ///
    /// # Errors
    ///
    /// If the underlying socket query fails.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.con.local_addr()
    }

    /// Borrow the underlying TCP stream.
    #[must_use]
    pub fn get_ref(&self) -> &TcpStream {
        &self.con
    }

    /// Mutably borrow the underlying TCP stream.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.con
    }

    /// Consume the wrapper and return the underlying TCP stream.
    #[must_use]
    pub fn into_inner(self) -> TcpStream {
        self.con
    }

    /// Receive the next AGW packet from the client.
    ///
    /// # Errors
    ///
    /// If the TCP stream fails or the AGW packet is malformed.
    pub async fn recv(&mut self) -> Result<Packet> {
        let mut header = [0_u8; HEADER_LEN];
        self.con.read_exact(&mut header).await?;
        let header = parse_header(&header)?;
        let payload = if header.data_len > 0 {
            let mut payload = vec![0; header.data_len as usize];
            self.con.read_exact(&mut payload).await?;
            payload
        } else {
            Vec::new()
        };
        Packet::parse(&header, &payload)
    }

    /// Send an AGW packet to the client.
    ///
    /// # Errors
    ///
    /// If the TCP stream fails.
    pub async fn send(&mut self, packet: &Packet) -> Result<()> {
        self.con.write_all(&packet.serialize()).await?;
        Ok(())
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
            con: Pipo::new(TcpStream::connect(addr).await?, r2)?,
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

    #[allow(clippy::too_many_arguments)]
    fn make_connection(
        &self,
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
        rule_handle: RuleHandle,
        rx: mpsc::Receiver<Packet>,
        server_state: Option<SharedServerConnectionState>,
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
            server_state,
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
                Ok(self.make_connection(
                    port,
                    pid,
                    src.clone(),
                    dst.clone(),
                    rule_handle,
                    rxd,
                    None,
                ))
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
            Some(pending.server_state),
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
    server_state: SharedServerConnectionState,
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
    server_state: Option<SharedServerConnectionState>,
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
        self.agw.send(packet.clone()).await?;
        self.buffer_server_data(&packet);
        Ok(())
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

    // Only accepted inbound connections participate in the replay logic.
    // Outbound connections and already-confirmed inbound ones leave this as a
    // no-op.
    fn buffer_server_data(&self, packet: &Packet) {
        let Some(server_state) = &self.server_state else {
            return;
        };
        if !matches!(packet, Packet::Data { .. }) {
            return;
        }
        let mut server_state = server_state.lock().unwrap();
        if !server_state.confirmed {
            server_state.buffered.push(packet.clone());
        }
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
                let packet = pending.packet.clone();
                self.pending_write = None;
                self.buffer_server_data(&packet);
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
    packet: Packet,
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
        let packet = this.data_packet(buf.to_vec());
        this.pending_write = Some(PendingWrite {
            len: buf.len(),
            packet: packet.clone(),
            fut: this.send_future(packet),
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

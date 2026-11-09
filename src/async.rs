// TODO: This code was a bit hastily written. With the benefit of hindsight it
// should be restructured with a more though through design.
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use log::{debug, trace};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpStream,
};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};

use crate::{parse_header, Call, CallsignHeard, Packet, Pid, Port, HEADER_LEN};
use crate::{Error, Result};

pub use crate::ViaHop;

const PID_AX25: Pid = Pid(0xf0);
const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);
const CALLSIGN_HEARD_REPLIES: usize = 20;
const CALLSIGN_HEARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

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
    ConnectionOutcome { port: Port, src: Call, dst: Call },
    IncomingConnect { port: Port, dst: Call },
    RegisterCallsign { call: Call },
    CallsignHeard { port: Port },
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
            RuleMatch::ConnectionOutcome { port, src, dst } => {
                if let Packet::ConnectionEstablished {
                    port: port2,
                    pid: _,
                    src: src2,
                    dst: dst2,
                }
                | Packet::ConnectionFailed {
                    port: port2,
                    pid: _,
                    src: src2,
                    dst: dst2,
                    message: _,
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
            RuleMatch::RegisterCallsign { call } => {
                if let Packet::RegisterCallsignReply { call: call2, .. } = packet {
                    return call == call2;
                }
            }
            RuleMatch::CallsignHeard { port } => {
                if let Packet::CallsignHeardReply {
                    port: response_port,
                    data: _,
                } = packet
                {
                    return port == response_port;
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
                        if let Err(e) = tx.send(packet.clone()).await {
                            // A route can disappear while this cloned snapshot
                            // is waiting for capacity. That ends only the route,
                            // not the shared AGW transport.
                            debug!("Route receiver send fail: {e}");
                            self.del(rule.ident);
                            continue;
                        }
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
                        if let Err(e) = tx
                            .send(PendingConnection {
                                port: *port,
                                pid: PID_AX25, // IncomingConnect always has pid 0x00.
                                src: dst.clone(),
                                dst: src.clone(),
                                rule_handle,
                                rx: rxd,
                                server_state,
                            })
                            .await
                        {
                            // Dropping the failed send also drops the pending
                            // connection's handle, removing its data route.
                            debug!("Route receiver PendingConnection send fail: {e}");
                            self.del(rule.ident);
                            continue;
                        }
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
    error: watch::Receiver<Option<Error>>,
    //rx: tokio::sync::Mutex<mpsc::Receiver<Packet>>,
}

impl Pipo {
    fn new(con: TcpStream, router: Arc<Router>) -> Result<Self> {
        //let (tx1, rx1) = mpsc::channel(10); // TODO: magic number.
        let (tx2, rx2) = mpsc::channel(10); // TODO: magic number.
        let (error_tx, error) = watch::channel(None);
        router.set_outgoing(tx2.clone())?;

        // TODO: probably should split this task in two.
        tokio::spawn(async move {
            if let Err(error) = Self::run(con, router, rx2).await {
                let _ = error_tx.send(Some(error));
            }
        });
        Ok(Pipo {
            tx: tx2,
            error,
            //rx: tokio::sync::Mutex::new(rx1),
        })
    }
    async fn send(&self, packet: Packet) -> Result<()> {
        self.tx.send(packet).await.map_err(Error::other)
    }

    fn error_receiver(&self) -> watch::Receiver<Option<Error>> {
        self.error.clone()
    }
    /*    async fn recv(&self) -> Option<Packet> {
        self.rx.lock().await.recv().await
    } */
    async fn run(con: TcpStream, router: Arc<Router>, rx: mpsc::Receiver<Packet>) -> Result<()> {
        let (reader, writer) = con.into_split();
        // While we could `try_join` here, that wouldn't shut down the
        // connection when the application disappears, and we'd be left hanging.
        tokio::select! {
            ret = Self::read_loop(reader, router) => ret,
            ret = Self::write_loop(writer, rx) => ret,
        }
    }

    async fn read_loop(mut con: OwnedReadHalf, router: Arc<Router>) -> Result<()> {
        loop {
            let mut header = [0_u8; HEADER_LEN];
            con.read_exact(&mut header).await?;
            let header = parse_header(&header)?;
            let payload = if header.data_len > 0 {
                let mut payload = vec![0; crate::payload_len(header.data_len)?];
                con.read_exact(&mut payload).await?;
                payload
            } else {
                Vec::new()
            };
            let packet = Packet::parse(&header, &payload)?;
            trace!("agw/pipo: Processing packet len {}", header.data_len);
            trace!("agw/pipo: Processing packet {packet:?}");
            router.process(packet).await?;
        }
    }

    async fn write_loop(mut con: OwnedWriteHalf, mut rx: mpsc::Receiver<Packet>) -> Result<()> {
        while let Some(packet) = rx.recv().await {
            let bytes = packet.serialize()?;
            con.write_all(&bytes).await?;
        }
        Ok(())
    }
}

struct ServerReceiveState {
    header_bytes: [u8; HEADER_LEN],
    header_read: usize,
    header: Option<crate::Header>,
    payload: Vec<u8>,
    payload_read: usize,
    error: Option<Error>,
}

impl Default for ServerReceiveState {
    fn default() -> Self {
        Self {
            header_bytes: [0; HEADER_LEN],
            header_read: 0,
            header: None,
            payload: Vec::new(),
            payload_read: 0,
            error: None,
        }
    }
}

impl ServerReceiveState {
    async fn recv(&mut self, read: &mut (impl AsyncRead + Unpin)) -> Result<Packet> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        // Only framing/transport errors are terminal. Once a complete frame
        // has been consumed, even a packet decoding error preserves alignment.
        if let Err(error) = self.read_frame(read).await {
            self.error = Some(error.clone());
            return Err(error);
        }
        let header = self.header.take().expect("complete frame has a header");
        self.header_read = 0;
        self.payload_read = 0;
        let packet = Packet::parse(&header, &self.payload);
        self.payload.clear();
        packet
    }

    async fn read_frame(&mut self, read: &mut (impl AsyncRead + Unpin)) -> Result<()> {
        while self.header_read < HEADER_LEN {
            let n = read
                .read(&mut self.header_bytes[self.header_read..])
                .await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.header_read += n;
        }
        if self.header.is_none() {
            let header = parse_header(&self.header_bytes)?;
            self.payload.resize(crate::payload_len(header.data_len)?, 0);
            self.header = Some(header);
        }
        while self.payload_read < self.payload.len() {
            let n = read.read(&mut self.payload[self.payload_read..]).await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.payload_read += n;
        }
        Ok(())
    }
}

/// Packet-oriented AGW server-side connection wrapper.
///
/// This is intended for code implementing an AGW server. It reads and writes
/// `Packet` values on a single accepted TCP stream without background tasks.
pub struct AGWServer {
    con: TcpStream,
    receive: ServerReceiveState,
}

impl AGWServer {
    /// Split ownership for dedicated reader and writer tasks.
    #[must_use]
    pub fn into_split(self) -> (AGWServerReader, AGWServerWriter) {
        let (read, write) = self.con.into_split();
        (
            AGWServerReader {
                read,
                receive: self.receive,
            },
            AGWServerWriter { write },
        )
    }
    /// Wrap an accepted AGW TCP connection.
    #[must_use]
    pub fn new(con: TcpStream) -> Self {
        Self {
            con,
            receive: ServerReceiveState::default(),
        }
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
    ///
    /// Do not interleave direct socket reads with `recv`: part of a frame
    /// may already be buffered by a cancelled receive.
    #[must_use]
    pub fn get_ref(&self) -> &TcpStream {
        &self.con
    }

    /// Mutably borrow the underlying TCP stream.
    ///
    /// Do not interleave direct socket reads with framed receives.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.con
    }

    /// Consume the wrapper and return the underlying TCP stream.
    /// Any partial frame buffered by a cancelled receive is discarded.
    #[must_use]
    pub fn into_inner(self) -> TcpStream {
        self.con
    }

    /// Receive the next AGW packet from the client.
    ///
    /// Cancellation safe: dropping this future preserves any partial frame
    /// so the next call resumes where it stopped. `into_split` also preserves
    /// this state.
    ///
    /// # Errors
    ///
    /// If the TCP stream fails or the AGW packet is malformed.
    /// Transport and framing errors are terminal and repeated on later calls.
    /// A decoding error for a complete frame allows reading the next frame.
    pub async fn recv(&mut self) -> Result<Packet> {
        self.receive.recv(&mut self.con).await
    }

    /// Send an AGW packet to the client.
    ///
    /// # Errors
    ///
    /// If the TCP stream fails.
    pub async fn send(&mut self, packet: &Packet) -> Result<()> {
        let bytes = packet.serialize()?;
        self.con.write_all(&bytes).await?;
        Ok(())
    }
}

/// Read half of an accepted AGW connection, retaining partial frames.
pub struct AGWServerReader {
    read: OwnedReadHalf,
    receive: ServerReceiveState,
}
impl AGWServerReader {
    /// Receive one frame. Cancellation safe: the next call resumes any partial
    /// frame consumed before this future was dropped.
    ///
    /// # Errors
    /// Returns transport and malformed frame errors.
    /// Transport and framing errors are terminal and repeated on later calls.
    /// A decoding error for a complete frame allows reading the next frame.
    pub async fn recv(&mut self) -> Result<Packet> {
        self.receive.recv(&mut self.read).await
    }
}
/// Write half of an accepted AGW connection.
pub struct AGWServerWriter {
    write: OwnedWriteHalf,
}

#[cfg(test)]
mod split_server_tests {
    use super::*;

    // Poll once to consume available bytes, then cancel exactly at Pending.
    // This avoids relying on timeouts or scheduler timing to trigger cancellation.
    async fn cancel_pending<T>(future: impl Future<Output = T>) {
        tokio::pin!(future);
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    async fn server_pair() -> (TcpStream, AGWServer) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let application = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        (application, AGWServer::new(socket))
    }

    #[tokio::test]
    async fn cancelled_server_receive_resumes_header_and_payload() {
        let (mut application, mut server) = server_pair().await;
        let packet = Packet::VersionReply {
            major: 2005,
            minor: 127,
        };
        let bytes = packet.serialize().unwrap();
        let mut start = 0;
        for end in [10, HEADER_LEN, HEADER_LEN + 3] {
            application.write_all(&bytes[start..end]).await.unwrap();
            server.con.readable().await.unwrap();
            cancel_pending(server.recv()).await;
            assert_eq!(server.receive.header_read, end.min(HEADER_LEN));
            assert_eq!(server.receive.payload_read, end.saturating_sub(HEADER_LEN));
            start = end;
        }
        application.write_all(&bytes[start..]).await.unwrap();
        application
            .write_all(&Packet::VersionQuery.serialize().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            server.recv().await.unwrap(),
            Packet::VersionReply {
                major: 2005,
                minor: 127
            }
        ));
        assert!(matches!(server.recv().await.unwrap(), Packet::VersionQuery));
    }

    #[tokio::test]
    async fn split_preserves_cancelled_receive_and_reader_can_resume() {
        let (mut application, mut server) = server_pair().await;
        let bytes = Packet::VersionReply {
            major: 2005,
            minor: 127,
        }
        .serialize()
        .unwrap();
        application.write_all(&bytes[..10]).await.unwrap();
        server.con.readable().await.unwrap();
        cancel_pending(server.recv()).await;
        assert_eq!(server.receive.header_read, 10);
        let (mut reader, _writer) = server.into_split();
        application
            .write_all(&bytes[10..HEADER_LEN + 2])
            .await
            .unwrap();
        reader.read.readable().await.unwrap();
        cancel_pending(reader.recv()).await;
        assert_eq!(reader.receive.payload_read, 2);
        application
            .write_all(&bytes[HEADER_LEN + 2..])
            .await
            .unwrap();
        application
            .write_all(&Packet::VersionQuery.serialize().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            reader.recv().await.unwrap(),
            Packet::VersionReply {
                major: 2005,
                minor: 127
            }
        ));
        assert!(matches!(reader.recv().await.unwrap(), Packet::VersionQuery));
    }

    #[tokio::test]
    async fn receive_errors_preserve_or_terminate_framing() {
        // EOF at either stage is terminal, even when recv is called again.
        let bytes = Packet::VersionReply { major: 1, minor: 2 }
            .serialize()
            .unwrap();
        for end in [0, 10, HEADER_LEN + 2] {
            let (mut writer, mut reader) = tokio::io::duplex(128);
            writer.write_all(&bytes[..end]).await.unwrap();
            drop(writer);
            let mut state = ServerReceiveState::default();
            let error = state.recv(&mut reader).await.unwrap_err().to_string();
            assert_eq!(
                state.recv(&mut reader).await.unwrap_err().to_string(),
                error
            );
        }

        // A complete but invalid R payload does not consume the following frame.
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let mut malformed = bytes[..HEADER_LEN].to_vec();
        malformed[28..32].copy_from_slice(&1_u32.to_le_bytes());
        malformed.push(0);
        writer.write_all(&malformed).await.unwrap();
        writer
            .write_all(&Packet::VersionQuery.serialize().unwrap())
            .await
            .unwrap();
        let mut state = ServerReceiveState::default();
        assert!(state.recv(&mut reader).await.is_err());
        assert!(matches!(
            state.recv(&mut reader).await.unwrap(),
            Packet::VersionQuery
        ));

        // An invalid length cannot establish a safe next-frame boundary.
        malformed[28..32].copy_from_slice(&(crate::MAX_PAYLOAD_LEN + 1).to_le_bytes());
        writer.write_all(&malformed[..HEADER_LEN]).await.unwrap();
        let error = state.recv(&mut reader).await.unwrap_err().to_string();
        assert_eq!(
            state.recv(&mut reader).await.unwrap_err().to_string(),
            error
        );
    }

    #[tokio::test]
    async fn partial_request_does_not_block_unsolicited_reply() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut application = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = AGWServer::new(socket).into_split();
        let request = Packet::VersionQuery.serialize().unwrap();
        application.write_all(&request[..10]).await.unwrap();
        let (received, ()) = tokio::join!(reader.recv(), async {
            writer
                .send(&Packet::VersionReply {
                    major: 2005,
                    minor: 127,
                })
                .await
                .unwrap();
            let mut bytes = [0; HEADER_LEN + 8];
            application.read_exact(&mut bytes).await.unwrap();
            assert!(matches!(
                Packet::parse(
                    &parse_header(bytes[..HEADER_LEN].try_into().unwrap()).unwrap(),
                    &bytes[HEADER_LEN..]
                )
                .unwrap(),
                Packet::VersionReply {
                    major: 2005,
                    minor: 127
                }
            ));
            application.write_all(&request[10..]).await.unwrap();
        });
        assert!(matches!(received.unwrap(), Packet::VersionQuery));
    }

    #[test]
    fn unregister_and_queue_wire_fixtures() {
        let mut header = [0; HEADER_LEN];
        header[4] = b'x';
        header[8..13].copy_from_slice(b"LOCAL");
        let packet = Packet::parse(&parse_header(&header).unwrap(), &[]).unwrap();
        assert!(matches!(
            packet,
            Packet::UnregisterCallsign(crate::Port(1), _)
        ));
        assert_eq!(packet.serialize().unwrap(), header);
        header[4] = b'Y';
        header[18..24].copy_from_slice(b"REMOTE");
        let query = Packet::parse(&parse_header(&header).unwrap(), &[]).unwrap();
        assert!(matches!(
            query,
            Packet::FramesOutstandingConnectionQuery { .. }
        ));
        assert_eq!(query.serialize().unwrap(), header);
        header[28] = 4;
        let packet =
            Packet::parse(&parse_header(&header).unwrap(), &[0x78, 0x56, 0x34, 0x12]).unwrap();
        assert!(matches!(
            packet,
            Packet::FramesOutstandingConnectionReply {
                count: 0x1234_5678,
                ..
            }
        ));
        let bytes = packet.serialize().unwrap();
        assert_eq!(&bytes[HEADER_LEN..], [0x78, 0x56, 0x34, 0x12]);
        assert!(Packet::parse(&parse_header(&header).unwrap(), &[1]).is_err());
    }
}
impl AGWServerWriter {
    /// Send one complete frame.
    ///
    /// # Errors
    /// Returns encoding and transport errors.
    pub async fn send(&mut self, packet: &Packet) -> Result<()> {
        self.write.write_all(&packet.serialize()?).await?;
        Ok(())
    }
}

pub struct AGW {
    con: Pipo,
    router: Arc<Router>,
    heard_query: AsyncMutex<()>,
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
            heard_query: AsyncMutex::new(()),
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

    /// Query callsigns recently heard on one AGW port.
    ///
    /// AGWPE replies with exactly twenty `H` frames. This method waits up to
    /// one second for all replies and serializes queries because their replies
    /// cannot identify the requesting client.
    ///
    /// # Cancellation safety
    ///
    /// If cancelling before all replies have been received, a retry could
    /// receive a mishmash of callsigns. Because the AGW protocol doesn't have
    /// request IDs, this is unavoidable in the general case, so just Don't Do
    /// That. The repercussions are not too bad, though.
    ///
    /// # Errors
    ///
    /// Returns an error if the AGW endpoint fails to send all responses.
    pub async fn callsign_heard(&self, port: Port) -> Result<Vec<CallsignHeard>> {
        let _query_lock = self.heard_query.lock().await;
        let (tx, mut rx) = mpsc::channel(CALLSIGN_HEARD_REPLIES);
        let _rule_handle = self.router.add(RuleMatch::CallsignHeard { port }, tx);
        self.send(Packet::CallsignHeardQuery(port)).await?;

        let deadline = tokio::time::Instant::now() + CALLSIGN_HEARD_TIMEOUT;
        let mut heard = Vec::new();
        for replies in 0..CALLSIGN_HEARD_REPLIES {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let packet = tokio::time::timeout(remaining, rx.recv())
                .await
                .map_err(|_| {
                    Error::msg(format!(
                        "timed out waiting for H replies after receiving {replies}"
                    ))
                })?
                .ok_or(Error::msg("AGW closed while waiting for H replies"))?;
            let Packet::CallsignHeardReply {
                port: response_port,
                data,
            } = packet
            else {
                return Err(Error::msg("unexpected packet while waiting for H replies"));
            };
            if response_port != port {
                return Err(Error::msg("H reply had an unexpected port"));
            }
            heard.append(&mut crate::v1::parse_callsign_heard(&data)?);
        }
        Ok(heard)
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
        let (tx, mut rx) = mpsc::channel(1);
        let rule_handle = self
            .router
            .add(RuleMatch::RegisterCallsign { call: src.clone() }, tx);
        self.send(Packet::RegisterCallsign(port, src.clone()))
            .await?;
        let reply = rx.recv().await.ok_or(Error::msg("no registration reply"))?;
        drop(rule_handle);
        match reply {
            Packet::RegisterCallsignReply { success: true, .. } => Ok(()),
            Packet::RegisterCallsignReply { success: false, .. } => Err(Error::msg(format!(
                "callsign registration failed for {src}"
            ))),
            other => Err(Error::msg(format!(
                "unexpected registration reply: {other:?}"
            ))),
        }
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
        via: &[Call],
    ) -> Result<Connection<'a>> {
        let connect_packet = if via.is_empty() {
            Packet::Connect {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
            }
        } else {
            Packet::ConnectVia {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
                via: via.to_vec(),
            }
        };
        self.connect_packet(port, pid, src, dst, connect_packet)
            .await
    }

    /// Connect through a route whose initial digipeaters have already been
    /// seen.
    ///
    /// Seen hops must form a contiguous prefix of the route. For example,
    /// `ViaHop::seen("WIDE1-1".parse()?)` may be followed by an unseen hop,
    /// but an unseen hop may not be followed by a seen hop.
    ///
    /// # Errors
    ///
    /// If the route is invalid or the underlying connection fails.
    pub async fn connect_via<'a>(
        &'a self,
        port: Port,
        pid: Pid,
        src: &Call,
        dst: &Call,
        via: &[ViaHop],
    ) -> Result<Connection<'a>> {
        let connect_packet = Packet::ConnectViaMarked {
            port,
            pid,
            src: src.clone(),
            dst: dst.clone(),
            via: via.to_vec(),
        };

        // Validate before registering connection rules or queuing the packet.
        // Pipo serializes in a background task, where an invalid route would
        // otherwise surface only as a transport failure.
        connect_packet.serialize()?;
        self.connect_packet(port, pid, src, dst, connect_packet)
            .await
    }

    async fn connect_packet<'a>(
        &'a self,
        port: Port,
        pid: Pid,
        src: &Call,
        dst: &Call,
        connect_packet: Packet,
    ) -> Result<Connection<'a>> {
        let (tx, mut rx) = mpsc::channel(1);

        // Register rule for receiving connection established.
        let ident = self.router.add(
            RuleMatch::ConnectionOutcome {
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
        if let Err(e) = self.send(connect_packet).await {
            return Err(Error::msg(format!("{e:?}")));
        }

        // Wait for connection established.
        let mut transport_error = self.con.error_receiver();
        let estab = tokio::time::timeout(CONNECTION_TIMEOUT, async {
            tokio::select! {
                reply = rx.recv() => reply.ok_or(Error::msg("no connection reply")),
                changed = transport_error.changed() => {
                    changed.map_err(Error::other)?;
                    Err(transport_error
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| Error::msg("AGW transport stopped")))
                }
            }
        })
        .await
        .map_err(|_| Error::msg("connection attempt timed out"))?;
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
            Packet::ConnectionFailed { message, .. } => {
                Err(Error::msg(format!("connection failed: {message}")))
            }
            other => Err(Error::msg(format!(
                "unexpected connection reply: {other:?}"
            ))),
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

// Retain only the capacity wait across polls. A cancelled write must never
// retain bytes that could be sent or counted against a subsequent buffer.
type PendingWrite = Pin<
    Box<
        dyn Future<
                Output = std::result::Result<mpsc::OwnedPermit<Packet>, mpsc::error::SendError<()>>,
            > + Send,
    >,
>;

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
        if buf.is_empty() {
            this.pending_write = None;
            return Poll::Ready(Ok(0));
        }
        if this.pending_write.is_none() {
            this.pending_write = Some(Box::pin(this.agw.con.tx.clone().reserve_owned()));
        }
        let permit = match this.pending_write.as_mut().unwrap().as_mut().poll(cx) {
            Poll::Ready(Ok(permit)) => permit,
            Poll::Ready(Err(e)) => {
                this.pending_write = None;
                this.disconnected = true;
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e.to_string(),
                )));
            }
            Poll::Pending => return Poll::Pending,
        };
        this.pending_write = None;
        let packet = this.data_packet(buf.to_vec());
        this.buffer_server_data(&packet);
        permit.send(packet);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // Completed writes are already queued; a pending reservation has no
        // accepted bytes to flush. Release it so other senders can progress.
        this.pending_write = None;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.pending_write = None;
        if this.disconnected && this.pending_shutdown.is_none() {
            return Poll::Ready(Ok(()));
        }
        if this.pending_shutdown.is_none() {
            this.pending_shutdown = Some(this.send_future(this.disconnect_packet()));
        }
        this.poll_pending_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn dropped_receiver_during_delivery_does_not_stop_other_routes() {
        let router = Arc::new(Router::new());
        let matcher = RuleMatch::Data {
            port: Port(1),
            src: "REMOTE".parse().unwrap(),
            dst: "LOCAL".parse().unwrap(),
        };
        let packet = Packet::Data {
            port: Port(1),
            pid: PID_AX25,
            src: "REMOTE".parse().unwrap(),
            dst: "LOCAL".parse().unwrap(),
            data: b"hello".to_vec(),
        };
        let (tx, rx) = mpsc::channel(1);
        tx.send(packet.clone()).await.unwrap();
        let handle = router.add(matcher.clone(), tx);
        let (live_tx, mut live_rx) = mpsc::channel(1);
        let _live_handle = router.add(matcher, live_tx);
        let delivery = router.process(packet.clone());
        tokio::pin!(delivery);
        std::future::poll_fn(|cx| {
            assert!(delivery.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Simulate cancellation of the task owning a Connection.
        drop(handle);
        drop(rx);
        assert!(delivery.await.unwrap());
        assert_eq!(live_rx.recv().await.unwrap(), packet);
        assert_eq!(router.rules.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dropped_listener_during_delivery_cleans_up_pending_connections() {
        let router = Arc::new(Router::new());
        let local: Call = "LOCAL".parse().unwrap();
        let (tx, rx) = mpsc::channel(1);
        let handle = router.add_incoming_listener(Port(1), local.clone(), tx);
        let incoming = |remote: &str| Packet::IncomingConnect {
            port: Port(1),
            pid: Pid(0),
            src: remote.parse().unwrap(),
            dst: local.clone(),
        };
        router.process(incoming("FIRST")).await.unwrap();
        let delivery = router.process(incoming("SECOND"));
        tokio::pin!(delivery);
        std::future::poll_fn(|cx| {
            assert!(delivery.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(handle);
        drop(rx);
        assert!(!delivery.await.unwrap());
        assert_eq!(router.rules.lock().unwrap().len(), 0);
        for remote in ["FIRST", "SECOND"] {
            assert!(router
                .get_server_connection(&ServerConnectionKey {
                    port: Port(1),
                    local: local.clone(),
                    remote: remote.parse().unwrap(),
                })
                .is_none());
        }
    }

    #[tokio::test]
    async fn stale_route_does_not_close_shared_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let agw = AGW::new(&listener.local_addr().unwrap().to_string())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let mut server = AGWServer::new(stream);
        let matcher = RuleMatch::Data {
            port: Port(1),
            src: "REMOTE".parse().unwrap(),
            dst: "LOCAL".parse().unwrap(),
        };
        let (tx, rx) = mpsc::channel(1);
        let _stale = agw.router.add(matcher.clone(), tx);
        drop(rx);
        let (tx, mut live_rx) = mpsc::channel(1);
        let _live = agw.router.add(matcher, tx);
        let packet = Packet::Data {
            port: Port(1),
            pid: PID_AX25,
            src: "REMOTE".parse().unwrap(),
            dst: "LOCAL".parse().unwrap(),
            data: b"hello".to_vec(),
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            server.send(&packet).await.unwrap();
            assert_eq!(live_rx.recv().await.unwrap(), packet);
            agw.send(Packet::VersionQuery).await.unwrap();
            assert_eq!(server.recv().await.unwrap(), Packet::VersionQuery);
            server.send(&packet).await.unwrap();
            assert_eq!(live_rx.recv().await.unwrap(), packet);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_write_uses_the_next_buffer() {
        // A controlled outgoing queue makes backpressure deterministic.
        for replacement in [b"XYZ".as_slice(), b"Z", b""] {
            let (tx, mut outgoing) = mpsc::channel(1);
            tx.send(Packet::VersionQuery).await.unwrap();
            let (_error_tx, error) = watch::channel(None);
            let agw = AGW {
                con: Pipo { tx, error },
                router: Arc::new(Router::new()),
                heard_query: AsyncMutex::new(()),
            };
            let (_tx, rx) = mpsc::channel(1);
            let state = Arc::new(Mutex::new(ServerConnectionState::default()));
            let mut con = agw.make_connection(
                Port(1),
                PID_AX25,
                "LOCAL".parse().unwrap(),
                "REMOTE".parse().unwrap(),
                RuleHandle::new(0, Weak::new()),
                rx,
                Some(state.clone()),
            );
            {
                let write = con.write(b"ABC");
                tokio::pin!(write);
                std::future::poll_fn(|cx| {
                    assert!(write.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            assert_eq!(outgoing.recv().await.unwrap(), Packet::VersionQuery);
            assert_eq!(con.write(replacement).await.unwrap(), replacement.len());
            if replacement.is_empty() {
                assert!(outgoing.try_recv().is_err());
                assert_eq!(state.lock().unwrap().buffered, Vec::<Packet>::new());
            } else {
                let packet = outgoing.recv().await.unwrap();
                assert!(matches!(&packet, Packet::Data { data, .. } if data == replacement));
                assert_eq!(state.lock().unwrap().buffered, vec![packet]);
            }
            assert!(outgoing.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn cancelled_write_is_not_sent_by_flush_or_shutdown() {
        for shutdown in [false, true] {
            let (tx, mut outgoing) = mpsc::channel(1);
            tx.send(Packet::VersionQuery).await.unwrap();
            let (_error_tx, error) = watch::channel(None);
            let agw = AGW {
                con: Pipo { tx, error },
                router: Arc::new(Router::new()),
                heard_query: AsyncMutex::new(()),
            };
            let (_tx, rx) = mpsc::channel(1);
            let mut con = agw.make_connection(
                Port(1),
                PID_AX25,
                "LOCAL".parse().unwrap(),
                "REMOTE".parse().unwrap(),
                RuleHandle::new(0, Weak::new()),
                rx,
                None,
            );
            {
                let write = con.write(b"ABC");
                tokio::pin!(write);
                std::future::poll_fn(|cx| {
                    assert!(write.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            if shutdown {
                assert_eq!(outgoing.recv().await.unwrap(), Packet::VersionQuery);
                con.shutdown().await.unwrap();
                assert!(matches!(
                    outgoing.recv().await.unwrap(),
                    Packet::Disconnect { .. }
                ));
            } else {
                // Flush must succeed even while the outgoing queue is full.
                con.flush().await.unwrap();
                assert_eq!(outgoing.recv().await.unwrap(), Packet::VersionQuery);
                outgoing.close();
                let error = con.write(b"Z").await.unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            }
            assert!(outgoing.try_recv().is_err());
        }
    }

    #[test]
    fn connection_outcome_rule_matches_failure() {
        let remote: Call = "REMOTE".parse().unwrap();
        let local: Call = "LOCAL".parse().unwrap();
        let rule = RuleMatch::ConnectionOutcome {
            port: Port(1),
            src: remote.clone(),
            dst: local.clone(),
        };

        assert!(rule.matches(&Packet::ConnectionFailed {
            port: Port(1),
            pid: Pid(0),
            src: remote,
            dst: local,
            message: "*** RETRYOUT".into(),
        }));
    }

    #[tokio::test]
    async fn callsign_heard_collects_twenty_replies() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut server = AGWServer::new(stream);
            assert_eq!(
                server.recv().await.unwrap(),
                Packet::CallsignHeardQuery(Port(1))
            );
            for data in [
                [
                    b"M0HEARD Mon,21Feb2000 11:14:30\0".as_slice(),
                    &[
                        0xd0, 0x07, 2, 0, 1, 0, 21, 0, 11, 0, 14, 0, 30, 0, 0, 0, 0xd0, 0x07, 2, 0,
                        1, 0, 21, 0, 12, 0, 18, 0, 22, 0, 0, 0,
                    ],
                ]
                .concat(),
                [
                    b"M0OTHER-3 Mon,21Feb2000 11:14:30\0".as_slice(),
                    &[
                        0xd0, 0x07, 2, 0, 1, 0, 21, 0, 11, 0, 14, 0, 30, 0, 0, 0, 0xd0, 0x07, 2, 0,
                        1, 0, 21, 0, 12, 0, 18, 0, 22, 0, 0, 0,
                    ],
                ]
                .concat(),
            ]
            .into_iter()
            .chain(std::iter::repeat_n(vec![0; 33], 18))
            {
                server
                    .send(&Packet::CallsignHeardReply {
                        port: Port(1),
                        data,
                    })
                    .await
                    .unwrap();
            }
        });

        let agw = AGW::new(&addr).await.unwrap();
        let heard = agw.callsign_heard(Port(1)).await.unwrap();
        assert_eq!(heard.len(), 2);
        assert_eq!(heard[0].call().to_string(), "M0HEARD");
        assert_eq!(heard[0].first_heard().year(), 2000);
        assert_eq!(heard[0].first_heard().hour(), 11);
        assert_eq!(heard[0].last_heard().minute(), 18);
        assert_eq!(heard[1].call().to_string(), "M0OTHER-3");
        server.await.unwrap();
    }
}

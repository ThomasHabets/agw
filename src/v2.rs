//! A synchronous, multiplexed AGW client.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use log::debug;

use crate::{Call, CallsignHeard, Error, Header, Packet, Pid, Port, PortCaps};
use crate::{PortsInfo, Reply, Result, HEADER_LEN};

const ROUTE_CAPACITY: usize = 64;

#[derive(Clone)]
struct Envelope {
    header: Header,
    reply: Reply,
}

enum RouteMatcher {
    Version,
    PortInfo,
    PortCaps(Port),
    CallsignHeard(Port),
    FramesOutstanding(Port),
    CallsignRegistration {
        port: Port,
        call: Call,
    },
    IncomingConnection {
        port: Port,
        local: Call,
    },
    Connection {
        port: Port,
        pid: Pid,
        local: Call,
        remote: Call,
    },
}

impl RouteMatcher {
    fn matches(&self, envelope: &Envelope) -> bool {
        match self {
            Self::Version => matches!(envelope.reply, Reply::Version(..)),
            Self::PortInfo => matches!(envelope.reply, Reply::PortInfo(..)),
            Self::PortCaps(port) => {
                matches!(envelope.reply, Reply::PortCaps(reply_port, _) if reply_port == *port)
            }
            Self::CallsignHeard(port) => {
                matches!(envelope.reply, Reply::CallsignHeard(reply_port, _) if reply_port == *port)
            }
            Self::FramesOutstanding(port) => matches!(
                envelope.reply,
                Reply::FramesOutstandingPort(reply_port, _) if reply_port == *port
            ),
            Self::CallsignRegistration { port, call } => {
                envelope.header.port == *port
                    && envelope.header.src.as_ref() == Some(call)
                    && matches!(envelope.reply, Reply::CallsignRegistration(..))
            }
            Self::IncomingConnection { port, local } => matches!(
                &envelope.reply,
                Reply::IncomingConnection(connection)
                    if connection.port == *port && connection.dst == *local
            ),
            Self::Connection {
                port,
                pid,
                local,
                remote,
            } => match &envelope.reply {
                Reply::ConnectionEstablished(connection) | Reply::ConnectionFailed(connection) => {
                    connection.port == *port
                        && connection.src == *remote
                        && connection.dst == *local
                }
                Reply::ConnectedData(data) => {
                    data.port == *port
                        && data.pid == *pid
                        && data.src == *remote
                        && data.dst == *local
                }
                // AGW disconnect notifications use PID zero even when data
                // uses another PID, so the callsigns identify the connection.
                Reply::Disconnect => {
                    envelope.header.port == *port
                        && envelope.header.src.as_ref() == Some(remote)
                        && envelope.header.dst.as_ref() == Some(local)
                }
                _ => false,
            },
        }
    }
}

struct Route {
    matcher: RouteMatcher,
    tx: Sender<Envelope>,
    terminal: Arc<Mutex<Option<Error>>>,
}

struct RouteReceiver {
    id: u64,
    parent: Arc<AgwCon>,
    rx: Receiver<Envelope>,
    terminal: Arc<Mutex<Option<Error>>>,
}

impl RouteReceiver {
    fn recv(&self) -> Result<Envelope> {
        self.rx.recv().map_err(|_| {
            self.terminal
                .lock()
                .expect("route terminal lock poisoned")
                .clone()
                .unwrap_or_else(|| Error::msg("AGW route closed"))
        })
    }
}

impl Drop for RouteReceiver {
    fn drop(&mut self) {
        self.parent.remove_route(self.id);
    }
}

struct AgwCon {
    next_route: AtomicU64,
    routes: Mutex<HashMap<u64, Route>>,
    txq: Mutex<Vec<u8>>,
    txq_notify: Condvar,
    shut_fd: std::os::fd::OwnedFd,
    exiting: AtomicBool,
}

impl AgwCon {
    fn new(shut_fd: std::os::fd::OwnedFd) -> Self {
        Self {
            next_route: AtomicU64::new(0),
            routes: Mutex::new(HashMap::new()),
            txq: Mutex::new(Vec::new()),
            txq_notify: Condvar::new(),
            shut_fd,
            exiting: AtomicBool::new(false),
        }
    }

    fn add_route(self: &Arc<Self>, matcher: RouteMatcher) -> RouteReceiver {
        let id = self.next_route.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = bounded(ROUTE_CAPACITY);
        let terminal = Arc::new(Mutex::new(None));
        self.routes.lock().expect("route lock poisoned").insert(
            id,
            Route {
                matcher,
                tx,
                terminal: Arc::clone(&terminal),
            },
        );
        RouteReceiver {
            id,
            parent: Arc::clone(self),
            rx,
            terminal,
        }
    }

    fn remove_route(&self, id: u64) {
        self.routes.lock().expect("route lock poisoned").remove(&id);
    }

    fn dispatch(&self, envelope: &Envelope) {
        let mut routes = self.routes.lock().expect("route lock poisoned");
        let mut full = Vec::new();
        for (id, route) in routes.iter() {
            if !route.matcher.matches(envelope) {
                continue;
            }
            match route.tx.try_send(envelope.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    *route.terminal.lock().expect("route terminal lock poisoned") =
                        Some(Error::msg("AGW route inbox overflow"));
                    full.push(*id);
                }
                Err(TrySendError::Disconnected(_)) => full.push(*id),
            }
        }
        for id in full {
            routes.remove(&id);
        }
    }

    fn fail_routes(&self, error: &Error) {
        let mut routes = self.routes.lock().expect("route lock poisoned");
        for route in routes.values() {
            *route.terminal.lock().expect("route terminal lock poisoned") = Some(error.clone());
        }
        routes.clear();
    }

    fn write(&self, data: &[u8]) -> Result<()> {
        if self.exiting.load(Ordering::Acquire) {
            return Err(Error::msg("AGW transport stopped"));
        }
        let mut txq = self.txq.lock()?;
        txq.extend_from_slice(data);
        self.txq_notify.notify_one();
        Ok(())
    }

    fn stop(&self) {
        if !self.exiting.swap(true, Ordering::AcqRel) {
            self.fail_routes(&Error::msg("AGW transport stopped"));
        }
        self.txq_notify.notify_all();
    }

    fn writer(&self, mut writer: impl Write) -> Result<()> {
        let mut txq = self.txq.lock()?;
        loop {
            while txq.is_empty() && !self.exiting.load(Ordering::Acquire) {
                txq = self.txq_notify.wait(txq)?;
            }
            if self.exiting.load(Ordering::Acquire) {
                return Ok(());
            }
            let written = writer.write(&txq)?;
            if written == 0 {
                return Err(Error::msg("AGW writer made no progress"));
            }
            txq.drain(..written);
        }
    }

    fn reader(&self, reader: impl Read + Poll) -> Result<()> {
        let result = self.reader_inner(reader);
        let error = result
            .as_ref()
            .err()
            .cloned()
            .unwrap_or_else(|| Error::msg("AGW transport stopped"));
        self.fail_routes(&error);
        self.exiting.store(true, Ordering::Release);
        self.txq_notify.notify_all();
        result
    }

    fn reader_inner(&self, mut reader: impl Read + Poll) -> Result<()> {
        use std::os::fd::AsRawFd;

        let mut bytes = [0_u8; HEADER_LEN];
        loop {
            if matches!(reader.poll(self.shut_fd.as_raw_fd())?, PollResult::Other) {
                return Ok(());
            }
            match reader.read_exact(&mut bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                result => result?,
            }
            let header = crate::parse_header(&bytes)?;
            let mut data = vec![0_u8; crate::payload_len(header.data_len)?];
            reader.read_exact(&mut data)?;
            let reply = crate::parse_reply(&header, &data)?;
            self.dispatch(&Envelope { header, reply });
        }
    }

    fn run<R: Read + Poll + Send, W: Write + Send>(&self, reader: R, writer: W) -> Result<()> {
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| self.reader(reader));
            let writer = scope.spawn(|| {
                let result = self.writer(writer);
                if result.is_err() {
                    self.stop();
                }
                result
            });
            let reader = reader
                .join()
                .map_err(|_| Error::msg("AGW reader thread panicked"))?;
            self.stop();
            let writer = writer
                .join()
                .map_err(|_| Error::msg("AGW writer thread panicked"))?;
            reader.and(writer)
        })
    }
}

/// A multiplexed AGW TCP client.
pub struct AGW {
    parent: Arc<AgwCon>,
    shut_fd: Mutex<Option<std::os::fd::OwnedFd>>,
    join_handle: Option<std::thread::JoinHandle<Result<()>>>,
    control: Mutex<()>,
    connecting: Mutex<()>,
}

/// Preferred name for the multiplexed AGW client.
pub type Client = AGW;

/// Parameters for one outgoing AX.25 connection.
pub struct ConnectRequest {
    port: Port,
    pid: Pid,
    local: Call,
    remote: Call,
    via: Vec<Call>,
}

impl ConnectRequest {
    #[must_use]
    pub fn new(port: Port, local: Call, remote: Call) -> Self {
        Self {
            port,
            pid: Pid(0xf0),
            local,
            remote,
            via: Vec::new(),
        }
    }

    #[must_use]
    pub fn pid(mut self, pid: Pid) -> Self {
        self.pid = pid;
        self
    }

    #[must_use]
    pub fn via(mut self, via: impl IntoIterator<Item = Call>) -> Self {
        self.via = via.into_iter().collect();
        self
    }
}

/// One event received from an AX.25 connection.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    Data(Vec<u8>),
    Disconnected,
}

/// An established AX.25 connection.
pub struct Connection {
    local: Call,
    remote: Call,
    port: Port,
    pid: Pid,
    connect_string: String,
    parent: Arc<AgwCon>,
    route: RouteReceiver,
    pending: VecDeque<Envelope>,
    read_buf: Vec<u8>,
    disconnected: bool,
}

/// A registration that accepts incoming AX.25 connections for one callsign.
pub struct Listener {
    port: Port,
    local: Call,
    parent: Arc<AgwCon>,
    route: RouteReceiver,
}

impl Listener {
    /// Wait for and accept the next incoming AX.25 connection.
    pub fn accept(&mut self) -> Result<Connection> {
        loop {
            let envelope = self.route.recv()?;
            let Reply::IncomingConnection(connection) = envelope.reply else {
                continue;
            };
            let remote = connection.src;
            let route = self.parent.add_route(RouteMatcher::Connection {
                port: self.port,
                pid: Pid(0xf0),
                local: self.local.clone(),
                remote: remote.clone(),
            });
            return Ok(Connection {
                local: self.local.clone(),
                remote,
                port: self.port,
                pid: Pid(0xf0),
                connect_string: connection.data,
                parent: Arc::clone(&self.parent),
                route,
                pending: VecDeque::new(),
                read_buf: Vec::new(),
                disconnected: false,
            });
        }
    }
}

impl Connection {
    #[must_use]
    pub fn local(&self) -> &Call {
        &self.local
    }

    #[must_use]
    pub fn remote(&self) -> &Call {
        &self.remote
    }

    #[must_use]
    pub fn port(&self) -> Port {
        self.port
    }

    #[must_use]
    pub fn pid(&self) -> Pid {
        self.pid
    }

    #[must_use]
    pub fn connect_string(&self) -> &str {
        &self.connect_string
    }

    /// Receive a complete AGW connected-data packet or a disconnect event.
    pub fn recv(&mut self) -> Result<Received> {
        if self.disconnected {
            return Ok(Received::Disconnected);
        }
        loop {
            let envelope = self
                .pending
                .pop_front()
                .map_or_else(|| self.route.recv(), Ok)?;
            match envelope.reply {
                Reply::ConnectedData(data) => return Ok(Received::Data(data.data)),
                Reply::Disconnect => {
                    self.disconnected = true;
                    return Ok(Received::Disconnected);
                }
                _ => {}
            }
        }
    }

    /// Send one connected-data packet.
    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        if self.disconnected {
            return Err(Error::msg("connection disconnected"));
        }
        self.parent.write(
            &Packet::Data {
                port: self.port,
                pid: self.pid,
                src: self.local.clone(),
                dst: self.remote.clone(),
                data: data.to_vec(),
            }
            .serialize()?,
        )
    }

    /// Request that the AGW endpoint close this AX.25 connection.
    pub fn disconnect(&mut self) -> Result<()> {
        if !self.disconnected {
            self.parent.write(
                &Packet::Disconnect {
                    port: self.port,
                    pid: self.pid,
                    src: self.local.clone(),
                    dst: self.remote.clone(),
                }
                .serialize()?,
            )?;
            self.disconnected = true;
        }
        Ok(())
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.read_buf.is_empty() {
            match self.recv().map_err(std::io::Error::other)? {
                Received::Data(data) => self.read_buf = data,
                Received::Disconnected => return Ok(0),
            }
        }
        let len = buf.len().min(self.read_buf.len());
        buf[..len].copy_from_slice(&self.read_buf[..len]);
        self.read_buf.drain(..len);
        Ok(len)
    }
}

impl Write for Connection {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.send(data).map_err(std::io::Error::other)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub enum PollResult {
    This,
    Other,
}

pub trait Poll {
    fn poll(&self, other: libc::c_int) -> Result<PollResult>;
}

impl<T: std::os::fd::AsFd> Poll for T {
    fn poll(&self, other: libc::c_int) -> Result<PollResult> {
        use std::os::fd::AsRawFd;

        loop {
            let mut fds = [
                libc::pollfd {
                    fd: self.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: other,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if fds[1].revents != 0 {
                return Ok(PollResult::Other);
            }
            if fds[0].revents != 0 {
                return Ok(PollResult::This);
            }
        }
    }
}

fn pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;

    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        Ok((
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        ))
    }
}

impl AGW {
    /// Connect to an AGW TCP endpoint.
    pub fn connect_tcp(addr: impl ToSocketAddrs) -> Result<Self> {
        let writer = TcpStream::connect(addr)?;
        let reader = writer.try_clone()?;
        Self::new(reader, writer)
    }

    /// Create a client from separate reader and writer transports.
    pub fn new<R: Read + Poll + Send + 'static, W: Write + Send + 'static>(
        reader: R,
        writer: W,
    ) -> Result<Self> {
        let (reader_stop, writer_stop) = pipe()?;
        let parent = Arc::new(AgwCon::new(reader_stop));
        let thread_parent = Arc::clone(&parent);
        let join_handle = std::thread::spawn(move || thread_parent.run(reader, writer));
        Ok(Self {
            parent,
            shut_fd: Mutex::new(Some(writer_stop)),
            join_handle: Some(join_handle),
            control: Mutex::new(()),
            connecting: Mutex::new(()),
        })
    }

    fn write_packet(&self, packet: &Packet) -> Result<()> {
        self.parent.write(&packet.serialize()?)
    }

    fn control<T>(
        &self,
        matcher: RouteMatcher,
        packet: &Packet,
        response: impl FnOnce(Reply) -> Result<T>,
    ) -> Result<T> {
        let _lock = self.control.lock()?;
        let route = self.parent.add_route(matcher);
        self.write_packet(packet)?;
        response(route.recv()?.reply)
    }

    pub fn stop(&self) {
        self.parent.stop();
        self.shut_fd.lock().expect("shutdown lock poisoned").take();
    }

    pub fn stop_wait(self) -> Result<()> {
        self.stop();
        self.wait()
    }

    pub fn wait(mut self) -> Result<()> {
        self.join_handle
            .take()
            .ok_or_else(|| Error::msg("wait called twice"))?
            .join()
            .map_err(|_| Error::msg("AGW transport thread panicked"))?
    }

    pub fn version(&self) -> Result<(u16, u16)> {
        self.control(
            RouteMatcher::Version,
            &Packet::VersionQuery,
            |reply| match reply {
                Reply::Version(major, minor) => Ok((major, minor)),
                _ => unreachable!("route matched another reply"),
            },
        )
    }

    pub fn port_info(&self) -> Result<PortsInfo> {
        self.control(
            RouteMatcher::PortInfo,
            &Packet::PortInfoQuery,
            |reply| match reply {
                Reply::PortInfo(info) => Ok(info),
                _ => unreachable!("route matched another reply"),
            },
        )
    }

    pub fn port_cap(&self, port: Port) -> Result<PortCaps> {
        self.control(
            RouteMatcher::PortCaps(port),
            &Packet::PortCapQuery(port),
            |reply| match reply {
                Reply::PortCaps(_, caps) => Ok(caps),
                _ => unreachable!("route matched another reply"),
            },
        )
    }

    pub fn callsign_heard(&self, port: Port) -> Result<Vec<CallsignHeard>> {
        self.control(
            RouteMatcher::CallsignHeard(port),
            &Packet::CallsignHeardQuery(port),
            |reply| match reply {
                Reply::CallsignHeard(_, heard) => Ok(heard),
                _ => unreachable!("route matched another reply"),
            },
        )
    }

    pub fn frames_outstanding(&self, port: Port) -> Result<usize> {
        self.control(
            RouteMatcher::FramesOutstanding(port),
            &Packet::FramesOutstandingPortQuery(port),
            |reply| match reply {
                Reply::FramesOutstandingPort(_, count) => Ok(count),
                _ => unreachable!("route matched another reply"),
            },
        )
    }

    pub fn register_callsign(&self, port: Port, call: &Call) -> Result<()> {
        self.control(
            RouteMatcher::CallsignRegistration {
                port,
                call: call.clone(),
            },
            &Packet::RegisterCallsign(port, call.clone()),
            |reply| match reply {
                Reply::CallsignRegistration(true) => Ok(()),
                Reply::CallsignRegistration(false) => Err(Error::msg(format!(
                    "callsign registration failed for {call}"
                ))),
                _ => unreachable!("route matched another reply"),
            },
        )
    }

    /// Register a callsign and accept incoming AX.25 connections to it.
    pub fn listen(&self, port: Port, local: &Call) -> Result<Listener> {
        let route = self.parent.add_route(RouteMatcher::IncomingConnection {
            port,
            local: local.clone(),
        });
        self.register_callsign(port, local)?;
        Ok(Listener {
            port,
            local: local.clone(),
            parent: Arc::clone(&self.parent),
            route,
        })
    }

    pub fn unproto(&self, port: Port, pid: Pid, src: &Call, dst: &Call, data: &[u8]) -> Result<()> {
        self.write_packet(&Packet::Unproto {
            port,
            pid,
            src: src.clone(),
            dst: dst.clone(),
            data: data.to_vec(),
        })
    }

    /// Open an AX.25 connection while retaining a dedicated inbound route.
    pub fn connect(&self, request: ConnectRequest) -> Result<Connection> {
        let _lock = self.connecting.lock()?;
        let matcher = RouteMatcher::Connection {
            port: request.port,
            pid: request.pid,
            local: request.local.clone(),
            remote: request.remote.clone(),
        };
        let route = self.parent.add_route(matcher);
        let packet = if request.via.is_empty() {
            Packet::Connect {
                port: request.port,
                pid: request.pid,
                src: request.local.clone(),
                dst: request.remote.clone(),
            }
        } else {
            Packet::ConnectVia {
                port: request.port,
                pid: request.pid,
                src: request.local.clone(),
                dst: request.remote.clone(),
                via: request.via.clone(),
            }
        };
        self.write_packet(&packet)?;
        let mut pending = VecDeque::new();
        let connect_string = loop {
            let envelope = route.recv()?;
            match envelope.reply {
                Reply::ConnectionEstablished(connection) => {
                    debug!("AGW connection confirmation uses PID {}", connection.pid.0);
                    break connection.data;
                }
                Reply::ConnectionFailed(connection) => {
                    return Err(Error::msg(format!(
                        "connection failed: {}",
                        connection.data
                    )));
                }
                Reply::Disconnect => {
                    return Err(Error::msg("connection disconnected during setup"))
                }
                _ => pending.push_back(envelope),
            }
        };
        debug!("AGW connected {} to {}", request.local, request.remote);
        Ok(Connection {
            local: request.local,
            remote: request.remote,
            port: request.port,
            pid: request.pid,
            connect_string,
            parent: Arc::clone(&self.parent),
            route,
            pending,
            read_buf: Vec::new(),
            disconnected: false,
        })
    }
}

impl Drop for AGW {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(value: &str) -> Call {
        value.parse().unwrap()
    }

    #[test]
    fn connection_route_retains_data_until_read() {
        let (reader_stop, _writer_stop) = pipe().unwrap();
        let parent = Arc::new(AgwCon::new(reader_stop));
        let local = call("LOCAL");
        let remote = call("REMOTE");
        let route = parent.add_route(RouteMatcher::Connection {
            port: Port(1),
            pid: Pid(0xf0),
            local: local.clone(),
            remote: remote.clone(),
        });
        parent.dispatch(&Envelope {
            header: Header::new(
                Port(1),
                b'D',
                Pid(0xf0),
                Some(remote.clone()),
                Some(local.clone()),
                2,
            ),
            reply: Reply::ConnectedData(crate::ConnectedData {
                port: Port(1),
                pid: Pid(0xf0),
                src: remote,
                dst: local,
                data: b"ok".to_vec(),
            }),
        });
        assert!(matches!(
            route.recv().unwrap().reply,
            Reply::ConnectedData(_)
        ));
    }

    #[test]
    fn connection_route_matches_zero_pid_disconnect() {
        let (reader_stop, _writer_stop) = pipe().unwrap();
        let parent = Arc::new(AgwCon::new(reader_stop));
        let local = call("LOCAL");
        let remote = call("REMOTE");
        let route = parent.add_route(RouteMatcher::Connection {
            port: Port(1),
            pid: Pid(0xf0),
            local: local.clone(),
            remote: remote.clone(),
        });
        parent.dispatch(&Envelope {
            header: Header::new(Port(1), b'd', Pid(0), Some(remote), Some(local), 0),
            reply: Reply::Disconnect,
        });
        assert!(matches!(route.recv().unwrap().reply, Reply::Disconnect));
    }
}

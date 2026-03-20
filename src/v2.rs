use log::{debug, warn};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use crate::Packet;
use crate::HEADER_LEN;
use crate::{Call, CallsignHeard, Pid, Port, Reply};
use crate::{Error, Result};
use crate::{PortCaps, PortsInfo};

struct Reader {
    parent: Arc<AgwCon>,
    id: u64,
    rx: std::sync::mpsc::Receiver<Reply>,
}

impl Reader {
    fn read(&self) -> Reply {
        self.rx.recv().expect("TODO")
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.parent.rx_off(self.id);
    }
}

struct AgwCon {
    id: std::sync::atomic::AtomicU64,
    children: Mutex<HashMap<u64, std::sync::mpsc::Sender<Reply>>>,
    // TODO: something better, like a rope or something?
    txq: Mutex<Vec<u8>>,
    txq_notify: std::sync::Condvar,

    shut_fd: std::os::fd::OwnedFd,
    exiting: std::sync::atomic::AtomicBool,
}

impl AgwCon {
    fn new(shut_fd: std::os::fd::OwnedFd) -> Self {
        Self {
            id: 0.into(),
            children: Mutex::new(HashMap::new()),
            txq: Mutex::new(vec![]),
            txq_notify: std::sync::Condvar::default(),
            exiting: false.into(),
            shut_fd,
        }
    }

    fn run<R: Poll + Read + Send, W: Write + Send>(&self, r: R, w: W) -> Result<()> {
        std::thread::scope(|s| {
            let jhr = s.spawn(move || {
                self.reader(r);
                debug!("Reader exited");
            });
            let jhw = s.spawn(move || {
                self.writer(w);
                debug!("Writer exited");
            });
            let jhr = jhr.join();
            let jhw = jhw.join();
            let ret = match (jhr, jhw) {
                (Ok(()), Ok(())) => Ok(()),
                (Ok(()), Err(e)) => Err(Error::msg(format!("write thread: {e:?}"))),
                (Err(e), Ok(())) => Err(Error::msg(format!("read thread: {e:?}"))),
                (Err(e1), Err(e2)) => Err(Error::msg(format!(
                    "read thread: {e1:?} write thread: {e2:?}"
                ))),
            };
            if let Err(ref e) = ret {
                warn!("AGW subthread error: {e:?}");
            }
            ret
        })
    }
    /// Write from application to AGW server.
    fn write(&self, data: &[u8]) -> Result<()> {
        let mut txq = self.txq.lock()?;
        txq.extend(data);
        self.txq_notify.notify_one();
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        todo!()
    }
    #[must_use]
    fn rx(self: &Arc<Self>) -> Reader {
        let id = self.id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.children.lock().unwrap().insert(id, tx);
        Reader {
            id,
            parent: Arc::clone(self),
            rx,
        }
    }
    fn rx_off(&self, id: u64) {
        self.children.lock().unwrap().remove(&id);
    }
    fn stop(&self) {
        self.exiting
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.txq_notify.notify_all();
    }
    fn writer(&self, mut w: impl Write) {
        let mut txq = self.txq.lock().unwrap();
        loop {
            while txq.is_empty() {
                txq = self.txq_notify.wait(txq).unwrap();
                if self.exiting.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }
            let n = w.write(&txq).expect("write failed");
            txq.drain(..n);
        }
    }
    fn reader(&self, r: impl Read + Poll) {
        match self.reader_inner(r) {
            Ok(()) => {} // TODO: send EOF to all children?
            Err(e) => {
                eprintln!("Reader error: {e}");
                let children = self.children.lock().unwrap();
                for child in children.values() {
                    if let Err(e) =
                        child.send(Reply::Error(Error::msg(format!("Reader eror: {e}"))))
                    {
                        warn!("Failed to write error to a subscribing client: {e}");
                    }
                }
            }
        }
    }
    fn reader_inner(&self, mut r: impl Read + Poll) -> Result<()> {
        use std::os::fd::AsRawFd;

        let mut header = [0_u8; HEADER_LEN];
        loop {
            // Poll for ready or done.
            // TODO: actually poll some pipe.
            if let PollResult::Other = r.poll(self.shut_fd.as_raw_fd())? {
                return Ok(());
            }
            // Read header.
            match r.read_exact(&mut header) {
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                other => other,
            }?;
            let header = crate::parse_header(&header)?;
            // Read data.
            let mut data = vec![0_u8; usize::try_from(header.data_len)?];
            r.read_exact(&mut data)?;

            // Inform all subscribing children.
            let reply = crate::parse_reply(&header, &data)?;
            let children = self.children.lock().unwrap();
            for child in children.values() {
                if let Err(e) = child.send(reply.clone()) {
                    warn!("Failed to write to a subscribing client: {e}");
                }
            }
        }
    }
}

/// AGW connection.
pub struct AGW {
    parent: Arc<AgwCon>,
    shut_fd: Mutex<Option<std::os::fd::OwnedFd>>,
    join_handle: Option<std::thread::JoinHandle<Result<()>>>,
}

pub struct Connection {
    me: Call,
    peer: Call,
    port: Port,
    pid: Pid,
    parent: Arc<AgwCon>,
    buf: Vec<u8>,
}

impl Write for Connection {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.parent.write(data).map_err(std::io::Error::other)?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.parent.flush().map_err(std::io::Error::other)
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.buf.is_empty() {
            let n = buf.len().min(self.buf.len());
            buf[..n].copy_from_slice(&self.buf);
            self.buf.drain(..n);
            return Ok(n);
        }
        let rx = self.parent.clone().rx();
        loop {
            match rx.read() {
                Reply::Error(e) => return Err(std::io::Error::other(e)),
                Reply::ConnectedData(d)
                    if d.src == self.peer
                        && d.dst == self.me
                        && d.port == self.port
                        && d.pid == self.pid =>
                {
                    self.buf.extend(&d.data);
                    // Reuse the code from above, even though it means an extra
                    // copy.
                    return self.read(buf);
                }
                _ => {}
            }
        }
    }
}

pub enum PollResult {
    This,
    Other,
}

pub trait Poll {
    fn poll(&self, other: libc::c_int) -> Result<PollResult>;
}
//impl Poll for std::net::TcpStream {
impl<T: std::os::fd::AsFd> Poll for T {
    fn poll(&self, other: libc::c_int) -> Result<PollResult> {
        use std::os::fd::AsRawFd;
        let fd = self.as_fd().as_raw_fd();
        loop {
            let mut fds = [
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: other,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if rc < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if rc == 0 {
                continue;
            }
            if rc == 2 {
                // Detault to saying other one is ready.
                return Ok(PollResult::Other);
            }
            if rc == 1 {
                if fds[0].revents & libc::POLLIN != 0 {
                    return Ok(PollResult::This);
                }
                if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    return Ok(PollResult::Other);
                }
                panic!("Can't happen: poll() returned 1, but nothing ready");
            }
            panic!("Can't happen: poll() returned {rc}");
        }
    }
}

fn pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        unsafe {
            Ok((
                std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                std::os::fd::OwnedFd::from_raw_fd(fds[1]),
            ))
        }
    }
}
impl AGW {
    /// Create AGW connection to ip:port.
    pub fn new<R: Poll + Read + Send + 'static, W: Write + Send + 'static>(
        r: R,
        w: W,
    ) -> Result<Self> {
        let (pr, pw) = pipe()?;
        let parent = Arc::new(AgwCon::new(pr));
        let p2 = parent.clone();
        let join_handle = std::thread::spawn(move || p2.run(r, w));
        Ok(Self {
            parent,
            shut_fd: Mutex::new(Some(pw)),
            join_handle: Some(join_handle),
        })
    }
    pub fn stop(&self) {
        self.parent.stop();
        self.shut_fd.lock().unwrap().take();
    }
    pub fn stop_wait(self) -> Result<()> {
        self.stop();
        self.wait()
    }
    pub fn wait(mut self) -> Result<()> {
        let jh = self
            .join_handle
            .take()
            .expect("can't happen: wait() called a second time");
        jh.join()
            .map_err(|e| Error::msg(format!("failed to join AGW thread: {e:?}")))?
    }

    /// Get AGW version.
    pub fn version(&self) -> Result<(u16, u16)> {
        let rx = self.parent.clone().rx();
        self.parent.write(&Packet::VersionQuery.serialize())?;
        loop {
            return match rx.read() {
                Reply::Error(e) => Err(e),
                Reply::Version(a, b) => Ok((a, b)),
                other => {
                    eprintln!("Got other: {other:?}");
                    continue;
                }
            };
        }
    }

    /// Get some port info for the AGW endpoint.
    pub fn port_info(&self) -> Result<PortsInfo> {
        let rx = self.parent.clone().rx();
        self.parent.write(&Packet::PortInfoQuery.serialize())?;
        loop {
            return match rx.read() {
                Reply::Error(e) => Err(e),
                Reply::PortInfo(i) => Ok(i),
                other => {
                    eprintln!("Got other: {other:?}");
                    continue;
                }
            };
        }
    }

    /// Get some port cap for the port.
    pub fn port_cap(&self, port: Port) -> Result<PortCaps> {
        let rx = self.parent.clone().rx();
        self.parent.write(&Packet::PortCapQuery(port).serialize())?;
        loop {
            return match rx.read() {
                Reply::Error(e) => Err(e),
                Reply::PortCaps(_port, caps) => Ok(caps),
                other => {
                    eprintln!("Got other: {other:?}");
                    continue;
                }
            };
        }
    }

    /// Get list of callsigns heard.
    pub fn callsign_heard(&self, port: Port) -> Result<Vec<CallsignHeard>> {
        let rx = self.parent.clone().rx();
        self.parent
            .write(&Packet::CallsignHeardQuery(port).serialize())?;
        loop {
            return match rx.read() {
                Reply::Error(e) => Err(e),
                Reply::CallsignHeard(_port, heard) => Ok(heard),
                other => {
                    eprintln!("Got other: {other:?}");
                    continue;
                }
            };
        }
    }

    /// Get list of callsigns heard.
    pub fn frames_outstanding(&self, port: Port) -> Result<usize> {
        let rx = self.parent.clone().rx();
        self.parent
            .write(&Packet::FramesOutstandingPortQuery(port).serialize())?;
        loop {
            return match rx.read() {
                Reply::Error(e) => Err(e),
                Reply::FramesOutstandingPort(_port, n) => Ok(n),
                other => {
                    eprintln!("Got other: {other:?}");
                    continue;
                }
            };
        }
    }

    /// Send UI packet.
    ///
    /// # Errors
    ///
    /// If the underlying connection fails.
    pub fn unproto(&self, port: Port, pid: Pid, src: &Call, dst: &Call, data: &[u8]) -> Result<()> {
        self.parent.write(
            &Packet::Unproto {
                port,
                pid,
                src: src.clone(),
                dst: dst.clone(),
                data: data.to_vec(),
            }
            .serialize(),
        )?;
        Ok(())
    }

    /// Register callsign.
    ///
    /// The specs say that registering the callsign is
    /// mandatory. Direwolf doesn't seem to care, but there it is.
    ///
    /// Presumably needed for incoming connection, but incoming
    /// connections are not tested yet.
    ///
    /// # Errors
    ///
    /// If underlying connection fails.
    pub fn register_callsign(&self, port: Port, src: &Call) -> Result<()> {
        debug!("Registering callsign");
        self.parent
            .write(&Packet::RegisterCallsign(port, src.clone()).serialize())?;
        Ok(())
    }

    pub fn connect(&self, port: Port, me: Call, peer: Call, via: &[Call]) -> Result<Connection> {
        let parent = self.parent.clone();
        let rx = parent.rx();
        let pid = Pid(0xF0);
        if via.is_empty() {
            self.parent.write(
                &Packet::Connect {
                    port,
                    pid,
                    src: me.clone(),
                    dst: peer.clone(),
                }
                .serialize(),
            )?;
        } else {
            self.parent.write(
                &Packet::ConnectVia {
                    port,
                    pid,
                    src: me.clone(),
                    dst: peer.clone(),
                    via: via.to_vec(),
                }
                .serialize(),
            )?;
            todo!();
        }
        let c = loop {
            break match rx.read() {
                Reply::Error(e) => Err(e),
                Reply::ConnectionEstablished(i) => Ok(i),
                _ => continue,
            };
        }?;
        debug!(
            "Connected with port {:?} pid {:?} src {:?} dst {:?} data {:?}",
            c.port, c.pid, c.src, c.dst, c.data
        );
        Ok(Connection {
            port,
            pid,
            me,
            peer,
            parent,
            buf: vec![],
        })
    }
}

impl Drop for AGW {
    fn drop(&mut self) {
        self.stop();
        // Don't wait for thread to exit. If you want to wait, call stop_wait().
    }
}

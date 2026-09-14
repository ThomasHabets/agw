use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    process::{ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

use anyhow::{Error, Result};
use clap::Parser;
use cursive::align::Align;
use cursive::theme::{Color, ColorStyle, ColorType};
use cursive::view::{Nameable, Resizable, ScrollStrategy};
use cursive::views::{
    Dialog, EditView, LinearLayout, ResizedView, ScrollView, TextContent, TextView,
};
use log::{debug, error, warn};
use serde::Serialize;

use agw::{Call, Pid, Port};

const DEFAULT_AGW_ADDR: &str = "127.0.0.1:8010";
const ZMODEM_START: &[u8] = b"**\x18B00";

enum TerminalConnection<'a> {
    Agw(agw::Connection<'a>),
    Tcp(TcpStream),
}

#[derive(Clone)]
enum TerminalWriter {
    Agw {
        sender: mpsc::Sender<Vec<u8>>,
        writer: agw::MakeWriter,
    },
    Tcp(mpsc::Sender<TcpWrite>),
}

enum TcpWrite {
    Data(Vec<u8>),
    Disconnect,
}

impl TerminalConnection<'_> {
    fn tcp(addr: &str) -> Result<Self> {
        Ok(Self::Tcp(TcpStream::connect(addr)?))
    }

    fn connect_string(&self) -> Result<String> {
        match self {
            Self::Agw(connection) => Ok(connection.connect_string().to_string()),
            Self::Tcp(stream) => Ok(format!("Connected to TCP {}", stream.peer_addr()?)),
        }
    }

    fn read(&mut self) -> Result<Vec<u8>> {
        match self {
            Self::Agw(connection) => connection.read().map_err(Error::from),
            Self::Tcp(stream) => {
                let mut data = vec![0; 1024];
                let len = stream.read(&mut data)?;
                if len == 0 {
                    return Err(Error::msg("TCP connection closed"));
                }
                data.truncate(len);
                Ok(data)
            }
        }
    }

    fn writer(&mut self) -> Result<TerminalWriter> {
        match self {
            Self::Agw(connection) => Ok(TerminalWriter::Agw {
                sender: connection.sender(),
                writer: connection.make_writer(),
            }),
            Self::Tcp(stream) => {
                let mut writer = stream.try_clone()?;
                let (sender, receiver) = mpsc::channel();
                std::thread::spawn(move || {
                    for write in receiver {
                        match write {
                            TcpWrite::Data(data) => {
                                if let Err(e) = writer.write_all(&data) {
                                    warn!("writing TCP data failed: {e}");
                                    return;
                                }
                            }
                            TcpWrite::Disconnect => {
                                if let Err(e) = writer.shutdown(Shutdown::Both) {
                                    warn!("closing TCP connection failed: {e}");
                                }
                                return;
                            }
                        }
                    }
                });
                Ok(TerminalWriter::Tcp(sender))
            }
        }
    }
}

impl TerminalWriter {
    fn send(&self, data: Vec<u8>) -> Result<()> {
        match self {
            Self::Agw { sender, writer } => {
                let packet = writer.data(data)?;
                sender
                    .send(packet)
                    .map_err(|e| Error::msg(format!("sending AGW data failed: {e}")))?;
            }
            Self::Tcp(sender) => sender
                .send(TcpWrite::Data(data))
                .map_err(|e| Error::msg(format!("sending TCP data failed: {e}")))?,
        }
        Ok(())
    }

    fn disconnect(&self) -> Result<()> {
        match self {
            Self::Agw { sender, writer } => sender
                .send(writer.disconnect())
                .map_err(|e| Error::msg(format!("sending AGW disconnect failed: {e}")))?,
            Self::Tcp(sender) => sender
                .send(TcpWrite::Disconnect)
                .map_err(|e| Error::msg(format!("closing TCP connection failed: {e}")))?,
        }
        Ok(())
    }
}

struct ZmodemReceiver {
    stdin: ChildStdin,
    completion: mpsc::Receiver<ZmodemExit>,
    cancel: mpsc::Sender<()>,
}

enum ZmodemExit {
    Exited(ExitStatus),
    Failed(String),
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
enum StatusUpdate {
    Message(String),
    Terminated(String),
}

#[derive(Default)]
struct RzProgressDecoder {
    line: String,
    escape: RzEscape,
}

#[derive(Default)]
enum RzEscape {
    #[default]
    None,
    Escape,
    Csi,
}

impl RzProgressDecoder {
    fn decode(&mut self, data: &[u8]) -> Option<String> {
        let mut latest = None;
        for &byte in data {
            match self.escape {
                RzEscape::Escape => {
                    self.escape = if byte == b'[' {
                        RzEscape::Csi
                    } else {
                        RzEscape::None
                    };
                    continue;
                }
                RzEscape::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape = RzEscape::None;
                    }
                    continue;
                }
                RzEscape::None => {}
            }

            match byte {
                b'\x1b' => self.escape = RzEscape::Escape,
                b'\r' | b'\n' => {
                    if !self.line.is_empty() {
                        latest = Some(self.line.clone());
                        self.line.clear();
                    }
                }
                b'\x08' => {
                    self.line.pop();
                    if !self.line.is_empty() {
                        latest = Some(self.line.clone());
                    }
                }
                0x20..=0x7e => {
                    self.line.push(byte.into());
                    latest = Some(self.line.clone());
                }
                _ => {}
            }
        }
        latest
    }

    fn finish(&mut self) -> Option<String> {
        (!self.line.is_empty()).then(|| std::mem::take(&mut self.line))
    }
}

#[allow(clippy::needless_pass_by_value)]
fn forward_rz_progress(mut stderr: impl Read, status_tx: mpsc::Sender<StatusUpdate>) {
    let mut decoder = RzProgressDecoder::default();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) => {
                warn!("reading rz progress failed: {e}");
                return;
            }
        };
        if let Some(status) = decoder.decode(&buffer[..read]) {
            if status_tx.send(StatusUpdate::Message(status)).is_err() {
                return;
            }
        }
    }
    if let Some(status) = decoder.finish() {
        let _ = status_tx.send(StatusUpdate::Message(status));
    }
}

impl ZmodemReceiver {
    #[allow(clippy::too_many_lines)]
    fn start(
        writer: TerminalWriter,
        active: Arc<AtomicBool>,
        status_tx: mpsc::Sender<StatusUpdate>,
    ) -> Result<Self> {
        let mut child = Command::new("rz")
            .args([
                "--binary",
                "-t",
                "1000", // 100 seconds.
                "--restricted",
                "--restricted",
                "--protect",
                "--zmodem",
                "--verbose",
                "--verbose",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::msg("rz did not provide stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::msg("rz did not provide stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::msg("rz did not provide stderr"))?;

        let _ = status_tx.send(StatusUpdate::Message("Receiving ZMODEM files".into()));

        std::thread::spawn(move || {
            let mut buf = [0_u8; 1024];
            loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) => {
                        warn!("reading rz output failed: {e}");
                        return;
                    }
                };
                if let Err(e) = writer.send(buf[..n].to_vec()) {
                    warn!("sending rz output failed: {e}");
                    return;
                }
            }
        });

        let progress_status_tx = status_tx.clone();
        let progress_reader = std::thread::spawn(move || {
            forward_rz_progress(stderr, progress_status_tx);
        });

        let (completion_tx, completion) = mpsc::channel();
        let (cancel, cancel_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut progress_reader = Some(progress_reader);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if let Some(reader) = progress_reader.take() {
                            if let Err(e) = reader.join() {
                                warn!("rz progress reader panicked: {e:?}");
                            }
                        }
                        active.store(false, Ordering::Release);
                        if status.success() {
                            let _ = status_tx
                                .send(StatusUpdate::Message("ZMODEM receive completed".into()));
                        } else {
                            let _ = status_tx.send(StatusUpdate::Message(format!(
                                "ZMODEM receive failed: {status}"
                            )));
                        }
                        let _ = completion_tx.send(ZmodemExit::Exited(status));
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        if let Some(reader) = progress_reader.take() {
                            if let Err(e) = reader.join() {
                                warn!("rz progress reader panicked: {e:?}");
                            }
                        }
                        active.store(false, Ordering::Release);
                        let message = format!("waiting for rz failed: {error}");
                        let _ = status_tx.send(StatusUpdate::Message(format!(
                            "ZMODEM receive failed: {message}"
                        )));
                        let _ = completion_tx.send(ZmodemExit::Failed(message));
                        return;
                    }
                }

                match cancel_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        if let Some(reader) = progress_reader.take() {
                            if let Err(e) = reader.join() {
                                warn!("rz progress reader panicked: {e:?}");
                            }
                        }
                        active.store(false, Ordering::Release);
                        let _ = status_tx
                            .send(StatusUpdate::Message("ZMODEM receive cancelled".into()));
                        let _ = completion_tx.send(ZmodemExit::Cancelled);
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });

        Ok(Self {
            stdin,
            completion,
            cancel,
        })
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.stdin.write_all(data)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn finished(&self) -> Result<bool> {
        match self.completion.try_recv() {
            Ok(ZmodemExit::Exited(status)) => {
                debug!("rz exited with {status}");
                Ok(true)
            }
            Ok(ZmodemExit::Failed(message)) => Err(Error::msg(message)),
            Ok(ZmodemExit::Cancelled) => Ok(true),
            Err(mpsc::TryRecvError::Empty) => Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(Error::msg("rz completion monitor stopped unexpectedly"))
            }
        }
    }
}

impl Drop for ZmodemReceiver {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
    }
}

fn zmodem_start_offset(data: &[u8]) -> Option<usize> {
    data.windows(ZMODEM_START.len())
        .position(|window| window == ZMODEM_START)
}

fn zmodem_start_prefix_len(data: &[u8]) -> usize {
    let max_len = data.len().min(ZMODEM_START.len() - 1);
    (1..=max_len)
        .rev()
        .find(|&len| data[data.len() - len..] == ZMODEM_START[..len])
        .unwrap_or(0)
}

#[derive(Default)]
struct TerminalTextDecoder {
    pending_carriage_return: bool,
}

impl TerminalTextDecoder {
    fn decode(&mut self, data: &[u8]) -> String {
        let mut plain = String::new();
        for &byte in data {
            if byte == 0 {
                continue;
            }
            if self.pending_carriage_return {
                self.pending_carriage_return = false;
                plain.push('\n');
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => self.pending_carriage_return = true,
                byte => plain.push((byte & 0x7f) as char),
            }
        }
        plain
    }

    fn finish(&mut self) -> String {
        if self.pending_carriage_return {
            self.pending_carriage_return = false;
            "\n".to_string()
        } else {
            String::new()
        }
    }
}

fn relay_terminal_data(
    data: &[u8],
    decoder: &mut TerminalTextDecoder,
    cq_tx: &mpsc::Sender<CQLogEntry>,
    down_tx: &mpsc::Sender<String>,
    src: &str,
    dst: &str,
) -> Result<bool> {
    relay_terminal_text(decoder.decode(data), cq_tx, down_tx, src, dst)
}

fn relay_terminal_text(
    plain: String,
    cq_tx: &mpsc::Sender<CQLogEntry>,
    down_tx: &mpsc::Sender<String>,
    src: &str,
    dst: &str,
) -> Result<bool> {
    if plain.is_empty() {
        return Ok(true);
    }
    cq_tx.send(CQLogEntry::message(CQLogEntryMessage {
        src: src.to_string(),
        dst: dst.to_string(),
        data: plain.clone(),
    }))?;
    if let Err(e) = down_tx.send(plain) {
        debug!("down_tx failed: {e}");
        return Ok(false);
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
fn run_ui(
    up_tx: mpsc::Sender<String>,
    down_rx: mpsc::Receiver<String>,
    status_rx: mpsc::Receiver<StatusUpdate>,
) {
    let mut siv = cursive::default();
    siv.set_fps(10);
    // siv.add_global_callback('q', |s| s.quit());

    let content = TextContent::new("");
    let content2 = content.clone();
    let initial_content = content.clone();
    std::thread::spawn(move || {
        for c in down_rx {
            // TODO: if adding new stuff, and not at bottom,
            // create a notification that gets dismissed when
            // at bottom.
            content2.append(c);
        }
    });

    let status = TextContent::new("");
    let status2 = status.clone();

    siv.set_window_title("AGW Terminal");
    siv.with_theme(|t| {
        //t.shadow = false;
        //t.borders = cursive::theme::BorderStyle::None;
        use cursive::theme::{
            BaseColor::White,
            Color::{Dark, Rgb},
            PaletteColor::{Primary, TitlePrimary, View},
        };
        // Full palette list from
        // https://docs.rs/cursive/0.20.0/cursive/theme/struct.Palette.html
        //   Background
        //   Shadow
        //   View
        //   Primary
        //   Secondary
        //   Tertiary
        //   TitlePrimary
        //   TitleSecondary
        //   Highlight
        //   HighlightInactive
        //   HighlightText
        t.palette[View] = Rgb(0, 0, 0);
        t.palette[Primary] = Dark(White);
        t.palette[TitlePrimary] = Rgb(255, 0, 0);
    });

    // Scroll view containing the data coming down from the other end.
    let scr = ScrollView::new(
        TextView::new_with_content(initial_content)
            .align(Align::top_left())
            .style(ColorStyle::new(
                ColorType::Color(Color::Rgb(200, 200, 200)),
                ColorType::Color(Color::Rgb(0, 0, 0)),
            ))
            .full_height(),
    )
    .scroll_strategy(ScrollStrategy::StickToBottom)
    .on_scroll(|s, _rect| {
        if s.call_on_name("scroll", |e: &mut ScrollView<ResizedView<TextView>>| {
            if e.is_at_bottom() {
                e.set_scroll_strategy(ScrollStrategy::StickToBottom);
            }
        })
        .is_none()
        {
            error!("Scroll is-at-bottom check callback failed to find the scroll view");
        }
    })
    .with_name("scroll");

    siv.add_fullscreen_layer(
        LinearLayout::vertical()
            .child(
                Dialog::around(
                    TextView::new_with_content(status)
                        .full_width()
                        .with_name("status"),
                )
                .title("Status"),
            )
            .child(scr)
            .child(
                Dialog::around(
                    EditView::new()
                        .on_submit(move |s, text| {
                            up_tx.send(text.to_owned() + "\r").expect("Sending command");
                            s.call_on_name("edit", |e: &mut EditView| {
                                e.set_content("");
                            })
                            .expect("call on name");
                        })
                        .style(ColorStyle::new(
                            ColorType::Color(Color::Rgb(0, 0, 0)),
                            ColorType::Color(Color::Rgb(200, 200, 200)),
                        ))
                        .with_name("edit"),
                )
                .title("input")
                .button("Quit", move |s| {
                    s.quit();
                }),
            )
            .full_screen(),
    );
    let status_sink = siv.cb_sink().clone();
    std::thread::spawn(move || {
        std::panic::set_hook(Box::new(|panic_info| {
            let backtrace = backtrace::Backtrace::new();
            error!("Status update thread panic: {panic_info:?}. Backtrace:");
            error!("{backtrace:?}");
        }));
        let mut terminated = false;
        for update in status_rx {
            let (text, is_terminated) = match update {
                StatusUpdate::Message(text) => (text, false),
                StatusUpdate::Terminated(text) => (text, true),
            };
            status2.set_content(ascii7_to_str(text.as_bytes()));
            if is_terminated && !terminated {
                terminated = true;
                if status_sink
                    .send(Box::new(|s| {
                        let _ = s.call_on_name("status", |view: &mut TextView| {
                            view.set_style(ColorStyle::new(
                                ColorType::Color(Color::Dark(cursive::theme::BaseColor::White)),
                                ColorType::Color(Color::Dark(cursive::theme::BaseColor::Red)),
                            ));
                        });
                    }))
                    .is_err()
                {
                    return;
                }
            }
        }
    });
    siv.run();
}

#[derive(Parser, Debug)]
struct Opts {
    // 0 -> Error 1 -> Warn 2 -> Info 3 -> Debug 4 or higher -> Trace
    // Default to INFO, because it won't log without being provided a logfile anyway.
    #[clap(short, default_value = "info")]
    verbose: String,

    #[clap(short)]
    log: Option<String>,

    #[clap(short = 'C', default_value = "/dev/null")]
    cq_log: String,

    #[clap(short, help = "AGW port number (default: 0)")]
    port: Option<u8>,

    // 240 = 0xF0
    #[clap(short = 'P', help = "AGW protocol ID (default: 240)")]
    pid: Option<u8>,

    #[clap(
        short = 'c',
        long = "agw-addr",
        conflicts_with = "tcp",
        help = "AGW endpoint (default: 127.0.0.1:8010)"
    )]
    agw_addr: Option<String>,

    #[clap(long, help = "Raw TCP endpoint")]
    tcp: Option<String>,

    src: Option<String>,
    dst: Option<String>,
}

enum ConnectionOptions {
    Agw {
        addr: String,
        port: Port,
        pid: Pid,
        src: Call,
        dst: Call,
    },
    Tcp {
        addr: String,
    },
}

impl Opts {
    fn connection_options(&self) -> Result<ConnectionOptions> {
        if let Some(addr) = &self.tcp {
            if self.port.is_some() || self.pid.is_some() {
                return Err(Error::msg("--port and --pid are only valid with AGW"));
            }
            if self.src.is_some() || self.dst.is_some() {
                return Err(Error::msg("SRC and DST are only valid with AGW"));
            }
            return Ok(ConnectionOptions::Tcp { addr: addr.clone() });
        }

        let src = self
            .src
            .as_deref()
            .ok_or_else(|| Error::msg("AGW connections require SRC and DST"))?
            .parse()?;
        let dst = self
            .dst
            .as_deref()
            .ok_or_else(|| Error::msg("AGW connections require SRC and DST"))?
            .parse()?;
        Ok(ConnectionOptions::Agw {
            addr: self
                .agw_addr
                .clone()
                .unwrap_or_else(|| DEFAULT_AGW_ADDR.to_string()),
            port: Port(self.port.unwrap_or(0)),
            pid: Pid(self.pid.unwrap_or(240)),
            src,
            dst,
        })
    }
}

#[derive(Serialize)]
struct CQLogEntryMessage {
    src: String,
    dst: String,
    data: String,
}

#[derive(Serialize)]
struct CQLogEntryMeta {
    msg: String,
}

#[derive(Serialize)]
struct CQLogEntry {
    timestamp: chrono::DateTime<chrono::Local>,

    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<CQLogEntryMeta>,

    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<CQLogEntryMessage>,
}
impl CQLogEntry {
    fn meta(msg: String) -> Self {
        Self {
            timestamp: chrono::Local::now(),
            message: None,
            meta: Some(CQLogEntryMeta { msg }),
        }
    }
    fn message(m: CQLogEntryMessage) -> Self {
        Self {
            timestamp: chrono::Local::now(),
            message: Some(m),
            meta: None,
        }
    }
}

fn cqlogthread_handle(logf: &mut std::fs::File, msg: &CQLogEntry) -> Result<()> {
    use std::io::Write;
    let serialized = serde_json::to_string(&msg)? + "\n";
    logf.write_all(serialized.as_bytes())?;
    Ok(())
}

fn cqlogthread(mut logf: std::fs::File, rx: mpsc::Receiver<CQLogEntry>) {
    if let Err(e) = cqlogthread_handle(&mut logf, &CQLogEntry::meta("Log opening".into())) {
        error!("Failed to log: {e}");
    }
    for msg in rx {
        if let Err(e) = cqlogthread_handle(&mut logf, &msg) {
            error!("Failed to log: {e}");
        }
    }
    if let Err(e) = cqlogthread_handle(&mut logf, &CQLogEntry::meta("Log closing".into())) {
        error!("Failed to log: {e}");
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::similar_names)]
fn main() -> Result<()> {
    let opt = Opts::parse();
    let connection_options = opt.connection_options()?;

    if let Some(logf) = opt.log {
        use std::io::Write;
        let target = Box::new(std::fs::File::create(logf).expect("Can't create log file {logf}"));
        let level = match opt.verbose.as_str() {
            "err" | "error" => log::LevelFilter::Error,
            "warn" | "warning" => log::LevelFilter::Warn,
            "info" => log::LevelFilter::Info,
            "debug" => log::LevelFilter::Debug,
            "trace" => log::LevelFilter::Trace,
            l => return Err(Error::msg(format!("Invalid log level {l}"))),
        };
        env_logger::Builder::new()
            .format(move |buf, record| {
                // ISO8601 / RFC3339 time format.
                const RFC3339: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";
                writeln!(
                    buf,
                    "{} {} {} {}:{} {}",
                    chrono::Local::now().format(RFC3339),
                    record.level(),
                    record.module_path().unwrap_or("unknown"),
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    record.args()
                )
            })
            .filter(Some(module_path!()), level)
            .filter(Some("agw"), level)
            .write_style(env_logger::WriteStyle::Never)
            .target(env_logger::Target::Pipe(target))
            .init();
    }
    log::info!("Terminal starting");

    let cqlogfile = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(opt.cq_log)?;

    let (cq_tx, cq_rx) = mpsc::channel();
    let cqloghandle = std::thread::spawn(move || {
        cqlogthread(cqlogfile, cq_rx);
    });

    let (up_tx, up_rx) = mpsc::channel();
    let (down_tx, down_rx) = mpsc::channel();
    let (status_tx, status_rx) = mpsc::channel();

    let mut agw_client = match &connection_options {
        ConnectionOptions::Agw { addr, .. } => Some(agw::AGW::new(addr)?),
        ConnectionOptions::Tcp { .. } => None,
    };
    let (mut con, local_label, remote_label) = match connection_options {
        ConnectionOptions::Agw {
            addr: _,
            port,
            pid,
            src,
            dst,
        } => {
            let agw = agw_client.as_mut().expect("AGW client was just created");
            agw.register_callsign(port, &src)?;
            let con = agw.connect(port, pid, &src, &dst, &[])?;
            (
                TerminalConnection::Agw(con),
                src.to_string(),
                dst.to_string(),
            )
        }
        ConnectionOptions::Tcp { addr } => {
            let con = TerminalConnection::tcp(&addr)?;
            let remote_label = con
                .connect_string()?
                .strip_prefix("Connected to TCP ")
                .unwrap_or(&addr)
                .to_string();
            (con, "TCP".to_string(), remote_label)
        }
    };
    let initial_status = con.connect_string()?;
    status_tx
        .send(StatusUpdate::Message(initial_status))
        .expect("sending initial status");
    let ui_thread = std::thread::spawn(move || {
        std::panic::set_hook(Box::new(|panic_info| {
            let backtrace = backtrace::Backtrace::new();
            error!("UI thread panic: {panic_info:?}. Backtrace:");
            error!("{backtrace:?}");
        }));
        run_ui(up_tx, down_rx, status_rx);
    });
    let terminal_writer = con.writer()?;
    let zmodem_writer = terminal_writer.clone();
    let zmodem_active = Arc::new(AtomicBool::new(false));

    let cq_tx2 = cq_tx.clone();
    let src2 = local_label.clone();
    let dst2 = remote_label.clone();
    let up_zmodem_active = Arc::clone(&zmodem_active);
    let up_status_tx = status_tx.clone();
    let up_writer = terminal_writer.clone();

    let up_thread = std::thread::spawn(move || loop {
        match up_rx.recv() {
            Ok(data) => {
                if up_zmodem_active.load(Ordering::Acquire) {
                    let _ = up_status_tx.send(StatusUpdate::Message(
                        "ZMODEM receive is still active; command not sent".into(),
                    ));
                    continue;
                }
                let bdata = data.as_bytes().to_vec();
                let _ = cq_tx2.send(CQLogEntry::message(CQLogEntryMessage {
                    src: src2.clone(),
                    dst: dst2.clone(),
                    data,
                }));
                if let Err(e) = up_writer.send(bdata) {
                    warn!("sending command failed: {e}");
                    let _ = up_status_tx.send(StatusUpdate::Terminated("Connection closed".into()));
                    return;
                }
            }
            Err(e) => {
                // UI exited.
                debug!("UI exited, up_rx got: {e}");
                if let Err(e) = up_writer.disconnect() {
                    debug!("disconnecting terminal failed: {e}");
                }
                return;
            }
        }
    });
    let mut zmodem_receiver: Option<ZmodemReceiver> = None;
    let mut zmodem_probe = Vec::new();
    let mut terminal_text = TerminalTextDecoder::default();
    loop {
        let read = match con.read() {
            Ok(data) => data,
            Err(e) => {
                if !relay_terminal_data(
                    &zmodem_probe,
                    &mut terminal_text,
                    &cq_tx,
                    &down_tx,
                    &remote_label,
                    &local_label,
                )? {
                    break;
                }
                if !relay_terminal_text(
                    terminal_text.finish(),
                    &cq_tx,
                    &down_tx,
                    &remote_label,
                    &local_label,
                )? {
                    break;
                }
                let _ = status_tx.send(StatusUpdate::Terminated("Connection closed".into()));
                debug!("Connection read: {e}");
                // TODO: update connected status box.
                break;
            }
        };

        let mut forward_to_zmodem = false;
        if let Some(receiver) = zmodem_receiver.as_mut() {
            match receiver.finished() {
                Ok(true) => zmodem_receiver = None,
                Ok(false) => match receiver.write(&read) {
                    Ok(()) => forward_to_zmodem = true,
                    Err(e) => {
                        // `rz` can exit just after the final ZMODEM frame.
                        // Do not discard this packet: it may already be the
                        // BBS's first ordinary response.
                        warn!("writing received data to rz failed: {e}");
                        zmodem_active.store(false, Ordering::Release);
                        zmodem_receiver = None;
                        let _ =
                            status_tx.send(StatusUpdate::Message("ZMODEM receive failed".into()));
                    }
                },
                Err(e) => {
                    warn!("checking rz status failed: {e}");
                    zmodem_active.store(false, Ordering::Release);
                    zmodem_receiver = None;
                    let _ = status_tx.send(StatusUpdate::Message("ZMODEM receive failed".into()));
                }
            }
        }
        if forward_to_zmodem {
            continue;
        }

        zmodem_probe.extend(read);
        if let Some(offset) = zmodem_start_offset(&zmodem_probe) {
            if !relay_terminal_data(
                &zmodem_probe[..offset],
                &mut terminal_text,
                &cq_tx,
                &down_tx,
                &remote_label,
                &local_label,
            )? {
                break;
            }
            let zmodem_data = zmodem_probe.split_off(offset);
            zmodem_probe.clear();
            zmodem_active.store(true, Ordering::Release);
            match ZmodemReceiver::start(
                zmodem_writer.clone(),
                Arc::clone(&zmodem_active),
                status_tx.clone(),
            ) {
                Ok(mut receiver) => {
                    if let Err(e) = receiver.write(&zmodem_data) {
                        warn!("writing ZMODEM header to rz failed: {e}");
                        zmodem_active.store(false, Ordering::Release);
                        let _ =
                            status_tx.send(StatusUpdate::Message("ZMODEM receive failed".into()));
                    } else {
                        zmodem_receiver = Some(receiver);
                    }
                }
                Err(e) => {
                    error!("starting rz failed: {e}");
                    zmodem_active.store(false, Ordering::Release);
                    let _ = status_tx.send(StatusUpdate::Message("Unable to start rz".into()));
                    if !relay_terminal_data(
                        &zmodem_data,
                        &mut terminal_text,
                        &cq_tx,
                        &down_tx,
                        &remote_label,
                        &local_label,
                    )? {
                        break;
                    }
                }
            }
        } else {
            let retained = zmodem_start_prefix_len(&zmodem_probe);
            let terminal_len = zmodem_probe.len() - retained;
            let terminal_data = zmodem_probe[..terminal_len].to_vec();
            zmodem_probe.drain(..terminal_len);
            if !relay_terminal_data(
                &terminal_data,
                &mut terminal_text,
                &cq_tx,
                &down_tx,
                &remote_label,
                &local_label,
            )? {
                break;
            }
        }
    }
    debug!("Joining UI and upload threads");
    up_thread.join().expect("up_thread join failed");
    if let Err(e) = ui_thread.join() {
        error!("UI thread crashed: {e:?}");
    }
    drop(cq_tx);
    cqloghandle.join().expect("CQ log thread failed");
    Ok(())
}

// TODO: smarter
fn ascii7_to_str(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in bytes {
        match b {
            0 => {}
            b => s.push((b & 0x7f) as char),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    #[test]
    fn finds_zmodem_start_sequence() {
        assert_eq!(zmodem_start_offset(b"text**\x18B000000000"), Some(4));
    }

    #[test]
    fn ignores_non_zmodem_terminal_data() {
        assert_eq!(zmodem_start_offset(b"sz some-file.txt\r"), None);
    }

    #[test]
    fn retains_only_a_possible_zmodem_start_prefix() {
        assert_eq!(zmodem_start_prefix_len(b"Disconnect"), 0);
        assert_eq!(zmodem_start_prefix_len(b"text**"), 2);
        assert_eq!(zmodem_start_prefix_len(b"**\x18B0"), 5);
    }

    #[test]
    fn normalizes_crlf_across_terminal_reads() {
        let mut decoder = TerminalTextDecoder::default();
        assert_eq!(decoder.decode(b"first\r"), "first");
        assert_eq!(decoder.decode(b"\nsecond\r\nthird"), "\nsecond\nthird");
        assert_eq!(decoder.finish(), "");
    }

    #[test]
    fn finishes_a_lone_carriage_return_as_a_newline() {
        let mut decoder = TerminalTextDecoder::default();
        assert_eq!(decoder.decode(b"last\r"), "last");
        assert_eq!(decoder.finish(), "\n");
    }

    #[test]
    fn rz_progress_keeps_only_the_current_printable_line() {
        let mut decoder = RzProgressDecoder::default();
        assert_eq!(
            decoder.decode(b"Receiving 10%\r"),
            Some("Receiving 10%".into())
        );
        assert_eq!(
            decoder.decode(b"\x1b[2KReceiving 20%\r"),
            Some("Receiving 20%".into())
        );
        assert_eq!(
            decoder.decode(b"Loading 10%\x08\x08\x0820%"),
            Some("Loading 20%".into())
        );
    }

    #[test]
    fn rz_progress_forwards_the_last_partial_line_at_eof() {
        let (status_tx, status_rx) = mpsc::channel();
        forward_rz_progress(Cursor::new(b"Receiving 10%\rReceiving 20%"), status_tx);
        assert_eq!(
            status_rx.try_iter().last(),
            Some(StatusUpdate::Message("Receiving 20%".into()))
        );
    }

    #[test]
    fn selects_tcp_without_agw_options() {
        let options = Opts::try_parse_from(["term", "--tcp", "127.0.0.1:23"])
            .expect("parsing TCP options")
            .connection_options()
            .expect("selecting TCP transport");
        assert!(matches!(options, ConnectionOptions::Tcp { .. }));

        let invalid = Opts::try_parse_from(["term", "--tcp", "127.0.0.1:23", "M0THC", "GB7CIP"])
            .expect("parsing TCP options with callsigns");
        assert!(invalid.connection_options().is_err());

        let invalid = Opts::try_parse_from(["term", "--tcp", "127.0.0.1:23", "-p", "0"])
            .expect("parsing TCP options with AGW port");
        assert!(invalid.connection_options().is_err());

        assert!(Opts::try_parse_from([
            "term",
            "--tcp",
            "127.0.0.1:23",
            "--agw-addr",
            "127.0.0.1:8010",
        ])
        .is_err());
    }

    #[test]
    fn uses_agw_defaults_when_tcp_is_not_requested() {
        let options = Opts::try_parse_from(["term", "M0THC", "GB7CIP"])
            .expect("parsing AGW options")
            .connection_options()
            .expect("selecting AGW transport");
        match options {
            ConnectionOptions::Agw {
                addr, port, pid, ..
            } => {
                assert_eq!(addr, DEFAULT_AGW_ADDR);
                assert_eq!(port, Port(0));
                assert_eq!(pid, Pid(240));
            }
            ConnectionOptions::Tcp { .. } => panic!("selected TCP instead of AGW"),
        }
    }

    #[test]
    fn tcp_transport_relays_raw_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding TCP listener");
        let addr = listener.local_addr().expect("reading listener address");
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accepting TCP client");
            stream.write_all(b"hello").expect("writing TCP data");
            let mut received = [0; 3];
            stream.read_exact(&mut received).expect("reading TCP data");
            assert_eq!(&received, b"bye");
        });

        let mut connection = TerminalConnection::tcp(&addr.to_string()).expect("connecting TCP");
        let writer = connection.writer().expect("creating TCP writer");
        assert_eq!(connection.read().expect("reading TCP data"), b"hello");
        writer.send(b"bye".to_vec()).expect("sending TCP data");
        writer.disconnect().expect("disconnecting TCP");
        peer.join().expect("TCP peer thread failed");
    }
}

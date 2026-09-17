use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{Error, Result};
use clap::Parser;
use cursive::align::Align;
use cursive::event::{Event as CursiveEvent, EventResult, Key};
use cursive::theme::{Color, ColorStyle, ColorType};
use cursive::view::{Nameable, Resizable, ScrollStrategy, View, ViewWrapper};
use cursive::views::{
    Dialog, EditView, EnableableView, LinearLayout, NamedView, OnEventView, ResizedView,
    ScrollView, TextContent, TextView,
};
use cursive::wrap_impl;
use cursive::Printer;
use log::{debug, error, warn};
use serde::Serialize;
use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

use agw::{Call, Pid, Port};

const DEFAULT_AGW_ADDR: &str = "127.0.0.1:8010";
const ZMODEM_START: &[u8] = b"**\x18B00";
const ZMODEM_TIMEOUT: Duration = Duration::from_secs(100);
const ZMODEM_UPLOAD_RATE_WINDOW: Duration = Duration::from_secs(5);
const ZMODEM_UPLOAD_STATUS_INTERVAL: Duration = Duration::from_millis(200);
const ZMODEM_UPLOAD_STATUS_REFRESH: Duration = Duration::from_secs(1);

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
                .send(writer.disconnect()?)
                .map_err(|e| Error::msg(format!("sending AGW disconnect failed: {e}")))?,
            Self::Tcp(sender) => sender
                .send(TcpWrite::Disconnect)
                .map_err(|e| Error::msg(format!("closing TCP connection failed: {e}")))?,
        }
        Ok(())
    }
}

struct ZmodemReceiver {
    input: mpsc::Sender<Vec<u8>>,
    completion: mpsc::Receiver<ZmodemExit>,
    cancel: mpsc::Sender<()>,
}

enum UiInput {
    Command(String),
    Upload(PathBuf),
}

type CommandEdit = OnEventView<NamedView<EditView>>;
type CommandInput = EnableableView<CommandEdit>;

fn delete_input_character<V: View>(input: &mut V) -> EventResult {
    input.on_event(CursiveEvent::Key(Key::Del))
}

/// An outgoing transfer. The selected local path is never sent as metadata:
/// only its basename is advertised to the BBS.
struct ZmodemSender {
    input: mpsc::Sender<Vec<u8>>,
    completion: mpsc::Receiver<()>,
}

/// Locally submitted payload progress. ZMODEM retransmissions can request an
/// earlier offset, so the display deliberately uses its high-water mark.
struct ZmodemUploadProgress {
    name: String,
    size: u32,
    sent: u64,
    started: Instant,
    samples: VecDeque<(Instant, u64)>,
    last_status: Instant,
}

impl ZmodemUploadProgress {
    fn new(name: String, size: u32, now: Instant) -> Self {
        Self {
            name,
            size,
            sent: 0,
            started: now,
            samples: VecDeque::from([(now, 0)]),
            last_status: now,
        }
    }

    fn record_submitted(&mut self, offset: Position, length: u64, now: Instant) -> bool {
        let end = u64::from(offset.get())
            .saturating_add(length)
            .min(u64::from(self.size));
        if end <= self.sent {
            return false;
        }
        self.sent = end;
        self.record_sample(now);
        true
    }

    fn should_report(&self, now: Instant) -> bool {
        now.duration_since(self.last_status) >= ZMODEM_UPLOAD_STATUS_INTERVAL
    }

    fn report(&mut self, status: &mpsc::Sender<StatusUpdate>, now: Instant) {
        let _ = status.send(StatusUpdate::Message(self.status(now)));
        self.last_status = now;
    }

    fn status(&mut self, now: Instant) -> String {
        self.record_sample(now);
        let (oldest_time, oldest_sent) = self.samples.front().expect("initial sample is kept");
        let (newest_time, newest_sent) = self.samples.back().expect("current sample is kept");
        let current_bps = bps(
            newest_sent.saturating_sub(*oldest_sent),
            newest_time.duration_since(*oldest_time),
        );
        let average_bps = bps(self.sent, now.duration_since(self.started));
        format!(
            "Uploading {}: {}/{} bytes sent locally \
             ({current_bps} bps current, {average_bps} bps avg)",
            self.name, self.sent, self.size
        )
    }

    fn record_sample(&mut self, now: Instant) {
        self.samples.push_back((now, self.sent));
        let Some(cutoff) = now.checked_sub(ZMODEM_UPLOAD_RATE_WINDOW) else {
            return;
        };
        while self.samples.len() > 1 && self.samples[1].0 <= cutoff {
            self.samples.pop_front();
        }
    }
}

fn bps(bytes: u64, elapsed: Duration) -> u128 {
    (u128::from(bytes) * 8_000) / elapsed.as_millis().max(1)
}

impl ZmodemSender {
    #[allow(clippy::too_many_lines)]
    fn start(
        path: PathBuf,
        writer: TerminalWriter,
        active: Arc<AtomicBool>,
        status: mpsc::Sender<StatusUpdate>,
    ) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::msg("upload path needs a UTF-8 file name"))?
            .to_owned();
        let size = u32::try_from(fs::metadata(&path)?.len())
            .map_err(|_| Error::msg("upload file is too large"))?;
        let (input, input_rx) = mpsc::channel();
        let (completion_tx, completion) = mpsc::channel();
        active.store(true, Ordering::Release);
        let _ = status.send(StatusUpdate::InputEnabled(false));

        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                use std::io::Seek;

                let mut file = File::open(path)?;
                let mut sender = Sender::new().map_err(|e| Error::msg(e.to_string()))?;
                sender
                    .start_file(FileInfo::new(name.as_bytes(), Some(Position::new(size))))
                    .map_err(|e| Error::msg(e.to_string()))?;
                let mut pending_wire = Vec::new();
                let mut session_completed = false;
                let started = Instant::now();
                let mut progress = ZmodemUploadProgress::new(name, size, started);
                progress.report(&status, started);
                let mut last_wire = started;

                loop {
                    match sender.poll() {
                        Action::WriteWire(bytes) => {
                            let bytes = bytes.to_vec();
                            writer.send(bytes.clone())?;
                            sender.wire_written(bytes.len());
                        }
                        Action::ReadFile { offset, max_len } => {
                            file.seek(std::io::SeekFrom::Start(u64::from(offset.get())))?;
                            let mut data = vec![0; max_len];
                            let read = file.read(&mut data)?;
                            if read == 0 {
                                return Err(Error::msg("upload file ended unexpectedly"));
                            }
                            sender
                                .submit_file(&data[..read])
                                .map_err(|e| Error::msg(e.to_string()))?;
                            let now = Instant::now();
                            if progress.record_submitted(
                                offset,
                                u64::try_from(read).map_err(Error::from)?,
                                now,
                            ) && progress.should_report(now)
                            {
                                progress.report(&status, now);
                            }
                        }
                        Action::Event(Event::FileCompleted) => {
                            sender.finish().map_err(|e| Error::msg(e.to_string()))?;
                            progress.report(&status, Instant::now());
                        }
                        Action::Event(Event::SessionCompleted) => session_completed = true,
                        Action::Event(Event::Aborted) => {
                            return Err(Error::msg("upload aborted"));
                        }
                        // Keep polling after SessionCompleted so the final
                        // ZFIN output is emitted before input is re-enabled.
                        Action::Idle if session_completed => return Ok(()),
                        Action::Idle => {
                            if !pending_wire.is_empty() {
                                let consumed = sender
                                    .submit_wire(&pending_wire)
                                    .map_err(|e| Error::msg(e.to_string()))?;
                                if consumed > 0 {
                                    pending_wire.drain(..consumed);
                                    continue;
                                }
                            }
                            match input_rx.recv_timeout(ZMODEM_UPLOAD_STATUS_REFRESH) {
                                Ok(data) => {
                                    last_wire = Instant::now();
                                    pending_wire.extend(data);
                                }
                                Err(mpsc::RecvTimeoutError::Timeout)
                                    if last_wire.elapsed() >= ZMODEM_TIMEOUT =>
                                {
                                    return Err(Error::msg("upload timed out"));
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {
                                    progress.report(&status, Instant::now());
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => {
                                    return Err(Error::msg("upload input closed"));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            })();

            active.store(false, Ordering::Release);
            let _ = status.send(StatusUpdate::InputEnabled(true));
            let _ = status.send(StatusUpdate::Message(result.map_or_else(
                |e| format!("ZMODEM upload failed: {e}"),
                |()| "ZMODEM upload completed".into(),
            )));
            let _ = completion_tx.send(());
        });
        Ok(Self { input, completion })
    }

    fn write(&self, data: &[u8]) -> Result<()> {
        self.input
            .send(data.to_vec())
            .map_err(|e| Error::msg(format!("sending ZMODEM data failed: {e}")))
    }

    fn finished(&self) -> bool {
        self.completion.try_recv().is_ok()
    }
}

enum ZmodemExit {
    Completed,
    Failed(String),
    Cancelled,
}

struct ZmodemFile {
    name: String,
    final_path: PathBuf,
    part_path: PathBuf,
    file: File,
    size: Option<u32>,
    written: u64,
    started: Instant,
    last_status: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum StatusUpdate {
    Message(String),
    Terminated(String),
    InputEnabled(bool),
}

struct StatusView {
    view: TextView,
    terminated: bool,
}

impl StatusView {
    fn new(content: TextContent) -> Self {
        Self {
            view: TextView::new_with_content(content),
            terminated: false,
        }
    }

    fn mark_terminated(&mut self) {
        self.terminated = true;
    }

    fn set_content(&mut self, content: String) {
        self.view.set_content(content);
    }
}

impl ViewWrapper for StatusView {
    wrap_impl!(self.view: TextView);

    fn wrap_draw(&self, printer: &Printer) {
        let style = if self.terminated {
            ColorStyle::new(
                ColorType::Color(Color::Dark(cursive::theme::BaseColor::White)),
                ColorType::Color(Color::Dark(cursive::theme::BaseColor::Red)),
            )
        } else {
            ColorStyle::primary()
        };
        printer.with_style(style, |printer| {
            let blank = " ".repeat(printer.size.x);
            for y in 0..printer.size.y {
                printer.print((0, y), &blank);
            }
            self.view.draw(printer);
        });
    }
}

impl ZmodemReceiver {
    fn start(
        writer: TerminalWriter,
        active: Arc<AtomicBool>,
        status_tx: mpsc::Sender<StatusUpdate>,
    ) -> Self {
        let (input, input_rx) = mpsc::channel();
        let (completion_tx, completion) = mpsc::channel();
        let (cancel, cancel_rx) = mpsc::channel();
        let _ = status_tx.send(StatusUpdate::InputEnabled(false));
        std::thread::spawn(move || {
            let result = run_zmodem_receiver(writer, input_rx, cancel_rx, &status_tx);
            active.store(false, Ordering::Release);
            let _ = status_tx.send(StatusUpdate::InputEnabled(true));
            let _ = completion_tx.send(result);
        });
        Self {
            input,
            completion,
            cancel,
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.input
            .send(data.to_vec())
            .map_err(|e| Error::msg(format!("sending ZMODEM data failed: {e}")))
    }

    fn finished(&self) -> Result<bool> {
        match self.completion.try_recv() {
            Ok(ZmodemExit::Completed) => {
                debug!("ZMODEM receive completed");
                Ok(true)
            }
            Ok(ZmodemExit::Failed(message)) => Err(Error::msg(message)),
            Ok(ZmodemExit::Cancelled) => Ok(true),
            Err(mpsc::TryRecvError::Empty) => Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(Error::msg("ZMODEM receiver stopped unexpectedly"))
            }
        }
    }
}

impl Drop for ZmodemReceiver {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_zmodem_receiver(
    writer: TerminalWriter,
    input: mpsc::Receiver<Vec<u8>>,
    cancel: mpsc::Receiver<()>,
    status_tx: &mpsc::Sender<StatusUpdate>,
) -> ZmodemExit {
    let mut receiver = match Receiver::with_flow_control(0, true) {
        Ok(receiver) => receiver,
        Err(e) => return ZmodemExit::Failed(format!("creating ZMODEM receiver failed: {e}")),
    };
    receiver.set_manual_file_accept(true);
    let _ = status_tx.send(StatusUpdate::Message("Receiving ZMODEM files".into()));
    let mut file = None;
    let mut session_completed = false;
    let mut last_wire = Instant::now();
    loop {
        match cancel.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                let _ = receiver.abort();
                let _ = drain_zmodem_actions(
                    &mut receiver,
                    &writer,
                    status_tx,
                    &mut file,
                    &mut session_completed,
                );
                let _ = status_tx.send(StatusUpdate::Message("ZMODEM receive cancelled".into()));
                return ZmodemExit::Cancelled;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match drain_zmodem_actions(
            &mut receiver,
            &writer,
            status_tx,
            &mut file,
            &mut session_completed,
        ) {
            Ok(Some(exit)) => return exit,
            Ok(None) => {}
            Err(e) => {
                let message = format!("ZMODEM receive failed: {e}");
                let _ = status_tx.send(StatusUpdate::Message(message.clone()));
                return ZmodemExit::Failed(message);
            }
        }
        match input.recv_timeout(Duration::from_secs(1)) {
            Ok(data) => {
                last_wire = Instant::now();
                let mut offset = 0;
                while offset < data.len() {
                    match receiver.submit_wire(&data[offset..]) {
                        Ok(0) => break,
                        Ok(consumed) => offset += consumed,
                        Err(e) => {
                            return ZmodemExit::Failed(format!(
                                "processing ZMODEM data failed: {e}"
                            ))
                        }
                    }
                    if let Ok(Some(exit)) = drain_zmodem_actions(
                        &mut receiver,
                        &writer,
                        status_tx,
                        &mut file,
                        &mut session_completed,
                    ) {
                        return exit;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) if last_wire.elapsed() >= ZMODEM_TIMEOUT => {
                if let Err(e) = receiver.timeout() {
                    return ZmodemExit::Failed(format!("ZMODEM timeout failed: {e}"));
                }
                last_wire = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return ZmodemExit::Cancelled,
        }
    }
}

fn drain_zmodem_actions(
    receiver: &mut Receiver,
    writer: &TerminalWriter,
    status_tx: &mpsc::Sender<StatusUpdate>,
    file: &mut Option<ZmodemFile>,
    session_completed: &mut bool,
) -> Result<Option<ZmodemExit>> {
    loop {
        match receiver.poll() {
            Action::WriteWire(bytes) => {
                let bytes = bytes.to_vec();
                writer.send(bytes.clone())?;
                receiver.wire_written(bytes.len());
            }
            Action::WriteFile(bytes) => {
                let bytes = bytes.to_vec();
                let current = file
                    .as_mut()
                    .ok_or_else(|| Error::msg("ZMODEM sent file data without a file"))?;
                current.file.write_all(&bytes)?;
                current.written += u64::try_from(bytes.len())?;
                receiver
                    .file_written(bytes.len())
                    .map_err(|e| Error::msg(e.to_string()))?;
                if current.last_status.elapsed() >= Duration::from_millis(200) {
                    current.last_status = Instant::now();
                    let _ = status_tx.send(StatusUpdate::Message(zmodem_progress(current)));
                }
            }
            Action::Event(Event::FileStarted(info)) => {
                let name = info.name.to_vec();
                let size = info.size.map(Into::into);
                let Some((name, final_path, part_path)) = zmodem_paths(&name)? else {
                    receiver
                        .skip_file()
                        .map_err(|e| Error::msg(e.to_string()))?;
                    let _ = status_tx.send(StatusUpdate::Message(
                        "Skipping unsafe or existing ZMODEM file".into(),
                    ));
                    continue;
                };
                let output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&part_path)?;
                receiver
                    .accept_file_at(0)
                    .map_err(|e| Error::msg(e.to_string()))?;
                let now = Instant::now();
                *file = Some(ZmodemFile {
                    name,
                    final_path,
                    part_path,
                    file: output,
                    size,
                    written: 0,
                    started: now,
                    last_status: now,
                });
                let _ = status_tx.send(StatusUpdate::Message(zmodem_progress(
                    file.as_ref().expect("ZMODEM file was just set"),
                )));
            }
            Action::Event(Event::FileCompleted) => {
                let current = file
                    .take()
                    .ok_or_else(|| Error::msg("ZMODEM completed an unknown file"))?;
                current.file.sync_all()?;
                if current.final_path.exists() {
                    return Err(Error::msg(format!(
                        "refusing to overwrite {}",
                        current.final_path.display()
                    )));
                }
                fs::rename(&current.part_path, &current.final_path)?;
                let _ = status_tx.send(StatusUpdate::Message(format!("Received {}", current.name)));
            }
            Action::Event(Event::SessionCompleted) => {
                // `zmodem2` reports the event before its final ZFIN has been
                // emitted.  Keep driving the receiver so ABBS can receive
                // that acknowledgement and send its final OO.
                *session_completed = true;
            }
            Action::Event(Event::Aborted) => {
                return Ok(Some(ZmodemExit::Failed(
                    "remote cancelled ZMODEM receive".into(),
                )))
            }
            Action::Event(_) | Action::ReadFile { .. } => {}
            Action::Idle if *session_completed => {
                let _ = status_tx.send(StatusUpdate::Message("ZMODEM receive completed".into()));
                return Ok(Some(ZmodemExit::Completed));
            }
            Action::Idle => return Ok(None),
            _ => return Err(Error::msg("unsupported ZMODEM receiver action")),
        }
    }
}

fn zmodem_paths(name: &[u8]) -> Result<Option<(String, PathBuf, PathBuf)>> {
    let Ok(name) = std::str::from_utf8(name) else {
        return Ok(None);
    };
    let path = Path::new(name);
    if name.is_empty()
        || path.components().count() != 1
        || path.file_name().is_none()
        || name == "."
        || name == ".."
    {
        return Ok(None);
    }
    let final_path = std::env::current_dir()?.join(name);
    let part_path = final_path.with_file_name(format!(".{name}.part"));
    if final_path.exists() || part_path.exists() {
        return Ok(None);
    }
    Ok(Some((name.into(), final_path, part_path)))
}

fn zmodem_progress(file: &ZmodemFile) -> String {
    let elapsed_ms = file.started.elapsed().as_millis().max(1);
    let rate = (u128::from(file.written) * 8000) / elapsed_ms;
    match file.size {
        Some(size) => format!(
            "Receiving {}: {}/{} bytes ({rate:.0} bps)",
            file.name, file.written, size
        ),
        None => format!(
            "Receiving {}: {} bytes ({rate:.0} bps)",
            file.name, file.written
        ),
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
    up_tx: mpsc::Sender<UiInput>,
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
    let connection_terminated = Arc::new(AtomicBool::new(false));
    let submit_terminated = Arc::clone(&connection_terminated);
    let command_tx = up_tx.clone();
    let upload_tx = up_tx;

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
                Dialog::around(StatusView::new(status).with_name("status").full_width())
                    .title("Status"),
            )
            .child(scr)
            .child(
                Dialog::around(
                    EnableableView::new(
                        OnEventView::new(
                            EditView::new()
                                .on_submit(move |s, text| {
                                    if submit_terminated.load(Ordering::Acquire) {
                                        return;
                                    }
                                    command_tx
                                        .send(UiInput::Command(text.to_owned() + "\r"))
                                        .expect("sending command");
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
                        .on_pre_event_inner(CursiveEvent::CtrlChar('d'), |input, _| {
                            Some(delete_input_character(input))
                        }),
                    )
                    .with_name("edit-container"),
                )
                .title("input")
                .button("ZMODEM upload", move |s| {
                    let upload_tx = upload_tx.clone();
                    s.add_layer(
                        Dialog::around(EditView::new().with_name("upload-path"))
                            .title("ZMODEM upload path")
                            .button("Upload", move |s| {
                                let path = s
                                    .call_on_name("upload-path", |view: &mut EditView| {
                                        view.get_content().to_string()
                                    })
                                    .expect("upload path input was just added");
                                if path.is_empty() {
                                    return;
                                }
                                if upload_tx.send(UiInput::Upload(PathBuf::from(path))).is_ok() {
                                    s.pop_layer();
                                }
                            })
                            .button("Cancel", |s| {
                                s.pop_layer();
                            }),
                    );
                })
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
            if let StatusUpdate::InputEnabled(enabled) = update {
                let enabled = enabled && !terminated;
                if status_sink
                    .send(Box::new(move |s| {
                        let _ = s.call_on_name("edit-container", |view: &mut CommandInput| {
                            view.set_enabled(enabled);
                        });
                        let _ = s.call_on_name("edit", |view: &mut EditView| {
                            if enabled {
                                view.enable();
                            } else {
                                view.disable();
                            }
                        });
                    }))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            let (text, is_terminated) = match update {
                StatusUpdate::Message(text) => (text, false),
                StatusUpdate::Terminated(text) => (text, true),
                StatusUpdate::InputEnabled(_) => unreachable!("handled above"),
            };
            let text = ascii7_to_str(text.as_bytes());
            let mark_terminated = is_terminated && !terminated;
            if mark_terminated {
                terminated = true;
                connection_terminated.store(true, Ordering::Release);
            }
            if status_sink
                .send(Box::new(move |s| {
                    let _ = s.call_on_name("status", |view: &mut StatusView| {
                        view.set_content(text);
                        if mark_terminated {
                            view.mark_terminated();
                        }
                    });
                    if mark_terminated {
                        let _ = s.call_on_name("edit-container", |view: &mut CommandInput| {
                            view.set_enabled(false);
                        });
                        let _ = s.call_on_name("edit", |view: &mut EditView| {
                            view.disable();
                        });
                    }
                }))
                .is_err()
            {
                return;
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
    let zmodem_sender = Arc::new(Mutex::new(None));

    let cq_tx2 = cq_tx.clone();
    let src2 = local_label.clone();
    let dst2 = remote_label.clone();
    let up_zmodem_active = Arc::clone(&zmodem_active);
    let up_zmodem_sender = Arc::clone(&zmodem_sender);
    let up_status_tx = status_tx.clone();
    let up_writer = terminal_writer.clone();

    let up_thread = std::thread::spawn(move || loop {
        match up_rx.recv() {
            Ok(UiInput::Command(data)) => {
                if up_zmodem_active.load(Ordering::Acquire) {
                    let _ = up_status_tx.send(StatusUpdate::Message(
                        "ZMODEM transfer is still active; command not sent".into(),
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
            Ok(UiInput::Upload(path)) => {
                if up_zmodem_active.load(Ordering::Acquire) {
                    let _ = up_status_tx.send(StatusUpdate::Message(
                        "ZMODEM transfer is still active; upload not started".into(),
                    ));
                    continue;
                }
                match ZmodemSender::start(
                    path,
                    up_writer.clone(),
                    Arc::clone(&up_zmodem_active),
                    up_status_tx.clone(),
                ) {
                    Ok(sender) => match up_zmodem_sender.lock() {
                        Ok(mut current) => *current = Some(sender),
                        Err(_) => {
                            let _ = up_status_tx.send(StatusUpdate::Message(
                                "ZMODEM upload could not be started".into(),
                            ));
                        }
                    },
                    Err(e) => {
                        let _ = up_status_tx
                            .send(StatusUpdate::Message(format!("ZMODEM upload failed: {e}")));
                    }
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
        match zmodem_sender.lock() {
            Ok(mut current) => {
                if let Some(sender) = current.as_ref() {
                    if sender.finished() {
                        *current = None;
                    } else {
                        sender.write(&read)?;
                        forward_to_zmodem = true;
                    }
                }
            }
            Err(_) => return Err(Error::msg("ZMODEM upload state was poisoned")),
        }
        if forward_to_zmodem {
            continue;
        }

        if let Some(receiver) = zmodem_receiver.as_mut() {
            match receiver.finished() {
                Ok(true) => zmodem_receiver = None,
                Ok(false) => match receiver.write(&read) {
                    Ok(()) => forward_to_zmodem = true,
                    Err(e) => {
                        // The receiver can finish just after the final ZMODEM frame.
                        // Do not discard this packet: it may already be the
                        // BBS's first ordinary response.
                        warn!("writing received data to ZMODEM failed: {e}");
                        zmodem_active.store(false, Ordering::Release);
                        zmodem_receiver = None;
                        let _ =
                            status_tx.send(StatusUpdate::Message("ZMODEM receive failed".into()));
                    }
                },
                Err(e) => {
                    warn!("checking ZMODEM status failed: {e}");
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
            let mut receiver = ZmodemReceiver::start(
                zmodem_writer.clone(),
                Arc::clone(&zmodem_active),
                status_tx.clone(),
            );
            if let Err(e) = receiver.write(&zmodem_data) {
                warn!("starting ZMODEM data failed: {e}");
                zmodem_active.store(false, Ordering::Release);
                let _ = status_tx.send(StatusUpdate::Message("ZMODEM receive failed".into()));
            } else {
                zmodem_receiver = Some(receiver);
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
    use std::net::TcpListener;

    #[test]
    fn ctrl_d_deletes_the_character_at_the_cursor() {
        let mut input = EditView::new().content("abc");
        input.set_cursor(1);

        delete_input_character(&mut input);

        assert_eq!(input.get_content().as_ref(), "ac");
        assert_eq!(input.get_cursor(), 1);
    }

    #[test]
    fn finds_zmodem_start_sequence() {
        assert_eq!(zmodem_start_offset(b"text**\x18B000000000"), Some(4));
    }

    #[test]
    fn ignores_non_zmodem_terminal_data() {
        assert_eq!(zmodem_start_offset(b"sz some-file.txt\r"), None);
    }

    #[test]
    fn upload_progress_uses_high_water_and_reports_bps() {
        let start = Instant::now();
        let mut progress = ZmodemUploadProgress::new("file.txt".into(), 1_000, start);
        assert!(progress.status(start).contains("0/1000 bytes sent locally"));

        assert!(progress.record_submitted(Position::new(0), 500, start + Duration::from_secs(1),));
        assert!(!progress.record_submitted(Position::new(0), 500, start + Duration::from_secs(2),));
        assert!(progress
            .status(start + Duration::from_secs(2))
            .contains("2000 bps current, 2000 bps avg"));
        assert!(progress
            .status(start + Duration::from_secs(7))
            .contains("0 bps current, 571 bps avg"));
    }

    #[test]
    fn rejects_unsafe_zmodem_filenames() {
        assert!(zmodem_paths(b"../escape").unwrap().is_none());
        assert!(zmodem_paths(b"subdir/file").unwrap().is_none());
        assert!(zmodem_paths(b"\xff").unwrap().is_none());
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

use std::str::FromStr;
use std::{
    io::{Read, Write},
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

const ZMODEM_START: &[u8] = b"**\x18B00";

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

impl ZmodemReceiver {
    fn start(
        sender: mpsc::Sender<Vec<u8>>,
        writer: agw::MakeWriter,
        active: Arc<AtomicBool>,
        status_tx: mpsc::Sender<String>,
    ) -> Result<Self> {
        let mut child = Command::new("rz")
            .args([
                "--binary",
                "--restricted",
                "--restricted",
                "--protect",
                "--zmodem",
                "--quiet",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::msg("rz did not provide stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::msg("rz did not provide stdout"))?;

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
                let packet = match writer.data(buf[..n].to_vec()) {
                    Ok(packet) => packet,
                    Err(e) => {
                        warn!("serializing rz output failed: {e}");
                        return;
                    }
                };
                if sender.send(packet).is_err() {
                    return;
                }
            }
        });

        let (completion_tx, completion) = mpsc::channel();
        let (cancel, cancel_rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    active.store(false, Ordering::Release);
                    if status.success() {
                        let _ = status_tx.send("ZMODEM receive completed".into());
                    } else {
                        let _ = status_tx.send(format!("ZMODEM receive failed: {status}"));
                    }
                    let _ = completion_tx.send(ZmodemExit::Exited(status));
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    active.store(false, Ordering::Release);
                    let message = format!("waiting for rz failed: {error}");
                    let _ = status_tx.send(format!("ZMODEM receive failed: {message}"));
                    let _ = completion_tx.send(ZmodemExit::Failed(message));
                    return;
                }
            }

            match cancel_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    active.store(false, Ordering::Release);
                    let _ = completion_tx.send(ZmodemExit::Cancelled);
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
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

fn relay_terminal_data(
    data: &[u8],
    cq_tx: &mpsc::Sender<CQLogEntry>,
    down_tx: &mpsc::Sender<String>,
    src: &str,
    dst: &str,
) -> Result<bool> {
    if data.is_empty() {
        return Ok(true);
    }
    let plain = ascii7_to_str(data);
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

fn run_ui(
    up_tx: mpsc::Sender<String>,
    down_rx: mpsc::Receiver<String>,
    status_rx: mpsc::Receiver<String>,
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
            .child(Dialog::around(TextView::new_with_content(status)).title("Status"))
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
    std::thread::spawn(move || {
        std::panic::set_hook(Box::new(|panic_info| {
            let backtrace = backtrace::Backtrace::new();
            error!("Status update thread panic: {panic_info:?}. Backtrace:");
            error!("{backtrace:?}");
        }));
        for c in status_rx {
            status2.set_content(ascii7_to_str(c.as_bytes()));
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

    #[clap(short, default_value = "0")]
    port: u8,

    // 240 = 0xF0
    #[clap(short = 'P', default_value = "240")]
    pid: u8,

    #[clap(short = 'c', default_value = "127.0.0.1:8010")]
    agw_addr: String,

    src: String,
    dst: String,
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

    let mut agw = agw::AGW::new(&opt.agw_addr)?;
    let src = &Call::from_str(&opt.src)?;
    let dst = &Call::from_str(&opt.dst)?;
    agw.register_callsign(Port(opt.port), src)?;
    let mut con = agw.connect(Port(opt.port), Pid(opt.pid), src, dst, &[])?;
    let initial_status: String = con.connect_string().into();
    status_tx
        .send(initial_status)
        .expect("sending initial status");
    let ui_thread = std::thread::spawn(move || {
        std::panic::set_hook(Box::new(|panic_info| {
            let backtrace = backtrace::Backtrace::new();
            error!("UI thread panic: {panic_info:?}. Backtrace:");
            error!("{backtrace:?}");
        }));
        run_ui(up_tx, down_rx, status_rx);
    });
    let sender = con.sender();
    let make_writer = con.make_writer();
    let zmodem_sender = sender.clone();
    let zmodem_writer = make_writer.clone();
    let zmodem_active = Arc::new(AtomicBool::new(false));

    let cq_tx2 = cq_tx.clone();
    let src2 = opt.src.clone();
    let dst2 = opt.dst.clone();
    let up_zmodem_active = Arc::clone(&zmodem_active);
    let up_status_tx = status_tx.clone();

    let up_thread = std::thread::spawn(move || loop {
        match up_rx.recv() {
            Ok(data) => {
                if up_zmodem_active.load(Ordering::Acquire) {
                    let _ = up_status_tx
                        .send("ZMODEM receive is still active; command not sent".into());
                    continue;
                }
                let bdata = data.as_bytes();
                let bdata = make_writer
                    .data(bdata)
                    .expect("failed to create user data packet");
                let _ = cq_tx2.send(CQLogEntry::message(CQLogEntryMessage {
                    src: src2.clone(),
                    dst: dst2.clone(),
                    data,
                }));
                sender.send(bdata).expect("sending command");
            }
            Err(e) => {
                // UI exited.
                debug!("UI exited, up_rx got: {e}");
                sender
                    .send(make_writer.disconnect())
                    .expect("failed to send disconnect");
                return;
            }
        }
    });
    let mut zmodem_receiver: Option<ZmodemReceiver> = None;
    let mut zmodem_probe = Vec::new();
    loop {
        let read = match con.read() {
            Ok(data) => data,
            Err(e) => {
                if !relay_terminal_data(&zmodem_probe, &cq_tx, &down_tx, &opt.dst, &opt.src)? {
                    break;
                }
                let _ = status_tx.send("Connection closed".into());
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
                        let _ = status_tx.send("ZMODEM receive failed".into());
                    }
                },
                Err(e) => {
                    warn!("checking rz status failed: {e}");
                    zmodem_active.store(false, Ordering::Release);
                    zmodem_receiver = None;
                    let _ = status_tx.send("ZMODEM receive failed".into());
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
                &cq_tx,
                &down_tx,
                &opt.dst,
                &opt.src,
            )? {
                break;
            }
            let zmodem_data = zmodem_probe.split_off(offset);
            zmodem_probe.clear();
            zmodem_active.store(true, Ordering::Release);
            match ZmodemReceiver::start(
                zmodem_sender.clone(),
                zmodem_writer.clone(),
                Arc::clone(&zmodem_active),
                status_tx.clone(),
            ) {
                Ok(mut receiver) => {
                    if let Err(e) = receiver.write(&zmodem_data) {
                        warn!("writing ZMODEM header to rz failed: {e}");
                        zmodem_active.store(false, Ordering::Release);
                        let _ = status_tx.send("ZMODEM receive failed".into());
                    } else {
                        zmodem_receiver = Some(receiver);
                        let _ = status_tx.send("Receiving ZMODEM files".into());
                    }
                }
                Err(e) => {
                    error!("starting rz failed: {e}");
                    zmodem_active.store(false, Ordering::Release);
                    let _ = status_tx.send("Unable to start rz".into());
                    if !relay_terminal_data(&zmodem_data, &cq_tx, &down_tx, &opt.dst, &opt.src)? {
                        break;
                    }
                }
            }
        } else {
            let retained = zmodem_start_prefix_len(&zmodem_probe);
            let terminal_len = zmodem_probe.len() - retained;
            let terminal_data = zmodem_probe[..terminal_len].to_vec();
            zmodem_probe.drain(..terminal_len);
            if !relay_terminal_data(&terminal_data, &cq_tx, &down_tx, &opt.dst, &opt.src)? {
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
}

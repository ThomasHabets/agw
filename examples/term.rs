use std::str::FromStr;
use std::sync::mpsc;

use anyhow::{Error, Result};
use clap::Parser;
use cursive::align::Align;
use cursive::theme::{Color, ColorStyle, ColorType};
use cursive::view::{Nameable, Resizable, ScrollStrategy};
use cursive::views::{
    Dialog, EditView, LinearLayout, ResizedView, ScrollView, TextContent, TextView,
};
use log::{debug, error};
use serde::Serialize;

use agw::Call;

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
        use cursive::theme::{BaseColor::*, Color::*, PaletteColor::*};
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
        if let None = s.call_on_name("scroll", |e: &mut ScrollView<ResizedView<TextView>>| {
            if e.is_at_bottom() {
                e.set_scroll_strategy(ScrollStrategy::StickToBottom);
            }
        }) {
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
                            .expect("call on name")
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
            error!("{:?}", backtrace);
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

fn cqlogthread_handle(logf: &mut std::fs::File, msg: CQLogEntry) -> Result<()> {
    use std::io::Write;
    let serialized = serde_json::to_string(&msg)? + "\n";
    logf.write_all(serialized.as_bytes())?;
    Ok(())
}

fn cqlogthread(mut logf: std::fs::File, rx: mpsc::Receiver<CQLogEntry>) {
    if let Err(e) = cqlogthread_handle(&mut logf, CQLogEntry::meta("Log opening".into())) {
        error!("Failed to log: {e}");
    }
    for msg in rx {
        if let Err(e) = cqlogthread_handle(&mut logf, msg) {
            error!("Failed to log: {e}");
        }
    }
    if let Err(e) = cqlogthread_handle(&mut logf, CQLogEntry::meta("Log closing".into())) {
        error!("Failed to log: {e}");
    }
}

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
                const RFC3339: &'static str = "%Y-%m-%dT%H:%M:%S%.3f%:z";
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
    agw.register_callsign(opt.port, opt.pid, src)?;
    let mut con = agw.connect(opt.port, opt.pid, src, dst, &[])?;
    let initial_status: String = con.connect_string().into();
    status_tx
        .send(initial_status)
        .expect("sending initial status");
    let ui_thread = std::thread::spawn(move || {
        std::panic::set_hook(Box::new(|panic_info| {
            let backtrace = backtrace::Backtrace::new();
            error!("UI thread panic: {panic_info:?}. Backtrace:");
            error!("{:?}", backtrace);
        }));
        run_ui(up_tx, down_rx, status_rx)
    });
    let sender = con.sender();
    // up
    let make_writer = con.make_writer();

    let cq_tx2 = cq_tx.clone();
    let src2 = opt.src.clone();
    let dst2 = opt.dst.clone();

    let up_thread = std::thread::spawn(move || loop {
        match up_rx.recv() {
            Ok(data) => {
                let bdata = data.as_bytes();
                let bdata = make_writer
                    .data(bdata)
                    .expect("failed to create user data packet");
                let _ = cq_tx2.send(CQLogEntry::message(CQLogEntryMessage {
                    src: src2.clone(),
                    dst: dst2.clone(),
                    data: data,
                }));
                sender.send(bdata).expect("sending command");
            }
            Err(e) => {
                // UI exited.
                debug!("UI exited, up_rx got: {}", e);
                sender
                    .send(make_writer.disconnect().expect("sending disconnect"))
                    .expect("failed to send disconnect");
                return;
            }
        };
    });
    // down
    loop {
        let read = match con.read() {
            Ok(data) => data,
            Err(e) => {
                let _ = status_tx.send("Connection closed".into());
                debug!("Connection read: {e}");
                // TODO: update connected status box.
                break;
            }
        };
        let plain = ascii7_to_str(&read);
        cq_tx.send(CQLogEntry::message(CQLogEntryMessage {
            src: opt.dst.clone(),
            dst: opt.src.clone(),
            data: plain.clone(),
        }))?;

        if let Err(e) = down_tx.send(plain) {
            debug!("down_tx failed: {}", e);
            break;
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
    for b in bytes.iter() {
        match b {
            0 => {}
            b => s.push((b & 0x7f) as char),
        };
    }
    s
}

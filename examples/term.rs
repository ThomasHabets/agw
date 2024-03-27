use agw::Call;
use anyhow::Result;
use clap::Parser;
use cursive::align::Align;
use cursive::theme::{Color, ColorStyle, ColorType};
use cursive::view::{Nameable, Resizable, ScrollStrategy};
use cursive::views::{Dialog, EditView, LinearLayout, ScrollView, TextContent, TextView};
use log::debug;
use std::sync::mpsc;

fn run_ui(up_tx: mpsc::Sender<String>, down_rx: mpsc::Receiver<String>) {
    let mut siv = cursive::default();
    siv.set_fps(10);
    // siv.add_global_callback('q', |s| s.quit());

    let content = TextContent::new("");
    let content2 = content.clone();
    let initial_content = content.clone();
    std::thread::spawn(move || {
        for c in down_rx {
            content2.append(c);
        }
    });
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

    siv.add_fullscreen_layer(
        LinearLayout::vertical()
            .child(Dialog::around(TextView::new("Connected or not?")).title("Status"))
            .child(
                ScrollView::new(
                    TextView::new_with_content(initial_content)
                        .align(Align::top_left())
                        .style(ColorStyle::new(
                            ColorType::Color(Color::Rgb(200, 200, 200)),
                            ColorType::Color(Color::Rgb(0, 0, 0)),
                        ))
                        .full_height(),
                )
                .scroll_strategy(ScrollStrategy::StickToBottom),
            )
            .child(
                Dialog::around(
                    EditView::new()
                        .on_submit(move |s, text| {
                            // TODO: if adding new stuff, and not at bottom,
                            // create a notification that gets dismissed when
                            // at bottom.
                            if false {
                                for _ in 0..1 {
                                    content.append(text.to_owned() + "\n");
                                }
                            }
                            up_tx.send(text.to_owned() + "\r").unwrap();
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
    siv.run();
}

#[derive(Parser, Debug)]
struct Cli {
    #[clap(short, default_value = "0")]
    verbose: usize,

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

fn main() -> Result<()> {
    let opt = Cli::parse();

    let (up_tx, up_rx) = mpsc::channel();
    let (down_tx, down_rx) = mpsc::channel();
    let ui_thread = std::thread::spawn(move || run_ui(up_tx, down_rx));

    let mut agw = agw::AGW::new(&opt.agw_addr)?;
    let src = &Call::from_str(&opt.src)?;
    let dst = &Call::from_str(&opt.dst)?;
    agw.register_callsign(opt.port, opt.pid, src)?;
    let mut con = agw.connect(opt.port, opt.pid, src, dst, &[])?;

    let sender = con.sender();
    // up
    let make_writer = con.make_writer();
    let up_thread = std::thread::spawn(move || loop {
        match up_rx.recv() {
            Ok(data) => {
                let data = data.as_bytes();
                let data = make_writer.make(data);
                sender.send(data).unwrap();
            }
            Err(e) => {
                // UI exited.
                debug!("UI exited, up_rx got {}", e);
                return;
            }
        };
    });
    // down
    loop {
        let read = con.read().expect("connection read");
        if let Err(e) = down_tx.send(ascii7_to_str(read)) {
            debug!("down_tx failed: {}", e);
            break;
        }
    }
    up_thread.join().expect("down_thread join failed");
    ui_thread.join().expect("thread not to crash");
    Ok(())
}

// TODO: smarter
fn ascii7_to_str(bytes: Vec<u8>) -> String {
    let mut s = String::new();
    for b in bytes.iter() {
        s.push((b & 0x7f) as char);
    }
    s
}

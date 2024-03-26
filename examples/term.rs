use cursive::align::Align;
use cursive::theme::{Color, ColorStyle, ColorType};
use cursive::view::{Nameable, Resizable, ScrollStrategy};
use cursive::views::{Dialog, EditView, LinearLayout, ScrollView, TextContent, TextView};
use std::sync::mpsc;
use clap::Parser;
use agw::Call;
use anyhow::Result;

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
                            for _ in 0..1 {
                                content.append(text.to_owned() + "\n");
                            }
			    up_tx.send(text.to_owned()).unwrap();
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

    #[clap(short = 'c', default_value = "127.0.0.1:8010")]
    agw_addr: String,

    src: String,
    dst: String,
}

fn main() -> Result<()>{
    let opt = Cli::parse();
    
    let (up_tx, up_rx) = mpsc::channel();
    let (down_tx, down_rx) = mpsc::channel();
    let ui_thread = std::thread::spawn(move || {
	run_ui(up_tx, down_rx)
    });

    let mut agw = agw::AGW::new(&opt.agw_addr)?;
    let src = &Call::from_str(&opt.src)?;
    let dst = &Call::from_str(&opt.dst)?;
    agw.register_callsign(0, 0xF0, src)?;
    let mut con = agw.connect(0, 0xF0, src, dst, &[])?;

    // down

    let down_thread = std::thread::spawn(move || {
	loop {
	    let read = con.read().expect("connection read");
	    if let Err(_) = down_tx.send(ascii7_to_str(read)) {
		break;
	    }
	}
    });
    // up
    loop {
	let msg = up_rx.recv().unwrap();
	con.write(msg.as_bytes())?;
    }
    down_thread.join().expect("down_thread join failed");
    
    panic!();
    ui_thread.join().expect("thread not to crash");
    Ok(())
}

fn ascii7_to_str(bytes: Vec<u8>) -> String {
    let mut s = String::new();
    for b in bytes.iter() {
        s.push((b & 0x7f) as char);
    }
    s
}

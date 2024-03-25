use cursive::align::Align;
use cursive::theme::{Color, ColorStyle, ColorType};
use cursive::view::{Nameable, Resizable, ScrollStrategy};
use cursive::views::{Dialog, EditView, LinearLayout, ScrollView, TextContent, TextView};

fn main() {
    let mut siv = cursive::default();

    // siv.add_global_callback('q', |s| s.quit());

    let content = TextContent::new("");
    let initial_content = content.clone();
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
                            for _ in 0..30 {
                                content.append(text.to_owned() + "\n");
                            }
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

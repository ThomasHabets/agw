use agw::Packet;
use anyhow::Result;
use clap::Parser;
use log::error;
use std::net::TcpListener;

#[derive(Parser, Debug)]
struct Opt {
    #[clap(short, default_value = "0")]
    verbose: usize,

    #[clap(short, long, default_value = "127.0.0.1:9011")]
    listen: String,

    #[clap(short = 'c', default_value = "127.0.0.1:8010")]
    agw_addr: String,
}

fn main() -> Result<()> {
    let opt = Opt::parse();
    stderrlog::new()
        .module(module_path!())
        .module("agw")
        .quiet(false)
        .verbosity(opt.verbose)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    //let mut agw = AGW::new(&opt.agw_addr)?;
    let listener = TcpListener::bind(&opt.listen)?;
    for stream in listener.incoming() {
        match stream {
            Err(e) => {
                error!("Failed to accept connection: {e}");
            }
            Ok(stream) => {
                std::thread::spawn(move || {
                    let mut s = agw::proxy::Proxy::new(stream).expect("Failed to create stream");
                    s.run(
                        &|packet: Packet| {
                            eprintln!("from server: {packet:?}");
                            packet
                        },
                        &|packet: Packet| {
                            eprintln!("from client: {packet:?}");
                            packet
                        },
                    )
                    .unwrap();
                    drop(s);
                });
            }
        }
    }
    Ok(())
}

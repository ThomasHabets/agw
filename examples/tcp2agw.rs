use agw::{Call, AGW};
use anyhow::Result;
use clap::Parser;
use log::error;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::str::FromStr;

#[derive(Parser, Debug)]
struct Opt {
    #[clap(short, default_value = "0")]
    verbose: usize,

    #[clap(short, long, default_value = "127.0.0.1:9011")]
    listen: String,

    #[clap(short = 'c', default_value = "127.0.0.1:8010")]
    agw_addr: String,

    #[clap(long, default_value = "0xF0")]
    pid: u8,

    #[clap()]
    src: String,

    #[clap()]
    dst: String,

    #[clap(short)]
    port: u8,
}

fn bidir(mut con: agw::Connection<'_>, mut stream: std::net::TcpStream) {
    let sender = con.sender();
    let writer = con.make_writer();

    // Up.
    {
        let mut stream = stream.try_clone().unwrap();
        std::thread::spawn(move || loop {
            let mut buf = [0_u8; 1024];
            match stream.read(&mut buf) {
                Ok(n) => {
                    let data = &buf[0..n].to_vec();
                    let data = writer
                        .data(data)
                        .expect("failed to create user data packet");
                    sender.send(data).expect("sending data");
                }
                Err(e) => {
                    error!("Error reading from TCP: {e:?}");
                }
            }
        });
    }

    // Down.
    loop {
        match con.read() {
            Ok(data) => stream.write_all(&data).unwrap(),
            Err(e) => {
                error!("Reading from AGWPE: {e:?} ");
            }
        }
    }
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

    let mut agw = AGW::new(&opt.agw_addr)?;
    let src = &Call::from_str(&opt.src).unwrap();
    let dst = &Call::from_str(&opt.dst).unwrap();
    agw.register_callsign(opt.port, opt.pid, &src)?;
    let con = agw.connect(opt.port, opt.pid, src, dst, &[])?;
    //let agw = Arc::new(Mutex::new(agw));
    let listener = TcpListener::bind(&opt.listen)?;
    //for stream in listener.incoming() {
    //let stream = stream?;
    let (stream, _) = listener.accept()?;
    //std::thread::spawn(move || {
    bidir(con, stream);
    //});
    //}
    Ok(())
}

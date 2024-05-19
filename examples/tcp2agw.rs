use agw::r#async::{Connection, AGW};
use agw::{Call, Packet};
use anyhow::Result;
use clap::Parser;
use log::info;
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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

async fn bidir(mut con: Connection<'_>, mut stream: TcpStream) -> Result<()> {
    loop {
        let mut buf = [0_u8; 1024];
        tokio::select! {
            data = con.recv() => {
            match data {
                Ok(Packet::Data{port: _, pid: _, src: _, dst: _, data}) => {
                stream.write_all(&data).await?;
                }
                Ok(other) => info!("Ignoring non-data packet {other:?}"),
                Err(e) => return Err(e),
            };
            },
            n = stream.read(&mut buf) => {
            let n = n?;
            let buf = &buf[0..n];
            con.send(buf).await?;
            },
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();
    stderrlog::new()
        .module(module_path!())
        .module("agw")
        .quiet(false)
        .verbosity(opt.verbose)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    let agw = AGW::new(&opt.agw_addr).await?;
    let src = &Call::from_str(&opt.src)?;
    let dst = &Call::from_str(&opt.dst)?;
    // agw.register_callsign(opt.port, opt.pid, &src)?;
    let con = agw.connect(opt.port, opt.pid, src, dst, &[]).await?;
    if false {
        let _con2 = agw.connect(opt.port, opt.pid, src, dst, &[]).await?;
    }
    //let agw = Arc::new(Mutex::new(agw));
    let listener = TcpListener::bind(&opt.listen).await?;
    //for stream in listener.incoming() {
    //let stream = stream?;
    let (stream, _) = listener.accept().await?;
    //std::thread::spawn(move || {
    bidir(con, stream).await?;
    //});
    //}
    Ok(())
}

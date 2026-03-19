use std::io::{Read, Write};
use std::str::FromStr;

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;

use agw::{Call, Port};

fn parse_port(s: &str) -> Result<Port, String> {
    let v: u8 = s
        .parse()
        .map_err(|_| format!("expected an integer in 0..=255, got {s:?}"))?;
    Ok(Port(v))
}

#[derive(Subcommand, Debug)]
enum Command {
    Connect {
        src: String,
        dst: String,
    },
    Version {},
    FramesOutstandingPort {
        #[arg(value_parser = parse_port)]
        port: Port,
    },
    PortInfo,
    PortCap {
        #[arg(value_parser = parse_port)]
        port: Port,
    },
    CallsignHeard {
        #[arg(value_parser = parse_port)]
        port: Port,
    },
    Unproto {
        src: String,
        dst: String,
        msg: String,
    },
}

#[derive(Parser, Debug)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[clap(short, default_value = "0")]
    verbose: usize,

    #[clap(short = 'c', default_value = "127.0.0.1:8010")]
    agw_addr: String,
}

fn main2(opt: Cli, agw: &agw::v2::AGW) -> Result<()> {
    match opt.command {
        Command::Version {} => {
            let (a, b) = agw.version()?;
            eprintln!("AGW server version: {a}.{b}");
        }
        Command::PortInfo => {
            let info = agw.port_info()?;
            println!("Port count: {}", info.count);
            for port in info.ports {
                println!("  {port:?}");
            }
        }
        Command::PortCap { port } => {
            let cap = agw.port_cap(port)?;
            println!("{cap:?}");
        }
        Command::CallsignHeard { port } => {
            let h = agw.callsign_heard(port)?;
            println!("{h:?}");
        }
        Command::FramesOutstandingPort { port } => {
            let n = agw.frames_outstanding(port)?;
            println!("Frames outstanding on {port:?}: {n}");
        }
        Command::Unproto { src, dst, msg } => {
            let pid = agw::Pid(0xF0); // TODO: make a flag.
            let port = agw::Port(0); // TODO
            agw.unproto(
                port,
                pid,
                &Call::from_str(&src)?,
                &Call::from_str(&dst)?,
                &msg.into_bytes(),
            )?;
        }
        Command::Connect { src, dst } => {
            let mut buf = [0u8; 128];

            let port = agw::Port(0); // TODO
            let src = &Call::from_str(&src)?;
            agw.register_callsign(port, src)?;
            let mut con = agw.connect(port, src.clone(), Call::from_str(&dst)?, &[])?;
            con.write_all(b"echo hello world\n")?;
            let data = {
                let n = con.read(&mut buf)?;
                &buf[..n]
            };
            eprintln!("Read: {:?}", ascii7_to_str(data));
            std::thread::sleep(std::time::Duration::from_secs(3));
            con.write_all(b"BYE\r")?;
            for _ in 0..10 {
                let data = {
                    let n = con.read(&mut buf)?;
                    &buf[..n]
                };
                eprintln!("Read: {:?}", ascii7_to_str(data));
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let opt = Cli::parse();
    stderrlog::new()
        .module(module_path!())
        .module("agw")
        .quiet(false)
        .verbosity(opt.verbose)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    let wstream = std::net::TcpStream::connect(&opt.agw_addr)?;
    let rstream = wstream.try_clone()?;
    let agw = agw::v2::AGW::new(rstream, wstream)?;
    main2(opt, &agw)?;

    // Optionally stop and wait.
    eprintln!("Closing");
    agw.stop_wait()?;
    //drop(agw);
    eprintln!("Thread joined");
    Ok(())
}

fn ascii7_to_str(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in bytes {
        s.push((b & 0x7f) as char);
    }
    s
}

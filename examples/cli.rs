use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use std::str::FromStr;

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
    PortInfo,
    PortCap {
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

    let mut agw = agw::AGW::new(&opt.agw_addr)?;

    match opt.command {
        Command::Version {} => {
            let (a, b) = agw.version()?;
            eprintln!("AGW server version: {a}.{b}");
        }
        Command::PortInfo => {
            let info = agw.port_info()?;
            println!("Port count: {}", info.count);
            for port in info.ports {
                println!("  {port}");
            }
        }
        Command::PortCap { port } => {
            let cap = agw.port_cap(port)?;
            println!("{cap:?}");
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
            let port = agw::Port(0); // TODO
            let pid = agw::Pid(0xF0); // TODO: make a flag.
            let src = &Call::from_str(&src)?;
            agw.register_callsign(port, pid, src)?;
            let mut con = agw.connect(port, pid, src, &Call::from_str(&dst)?, &[])?;
            con.write(b"echo hello world\n")?;
            eprintln!("Read: {:?}", ascii7_to_str(con.read()?));
            std::thread::sleep(std::time::Duration::from_millis(3000));
            con.write(b"BYE\r")?;
            for _ in 0..10 {
                eprintln!("Read: {:?}", ascii7_to_str(con.read()?));
            }
            con.disconnect()?;
        }
    }
    Ok(())
}

fn ascii7_to_str(bytes: Vec<u8>) -> String {
    let mut s = String::new();
    for b in bytes.iter() {
        s.push((b & 0x7f) as char);
    }
    s
}

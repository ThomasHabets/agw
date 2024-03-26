use agw::Call;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
enum Command {
    Connect {
        src: String,
        dst: String,
    },
    Version {},
    PortInfo {
        port: u8,
    },
    PortCap {
        port: u8,
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
        Command::PortInfo { port } => eprintln!("{}", agw.port_info(port)?),
        Command::PortCap { port } => eprintln!("{}", agw.port_cap(port)?),
        Command::Unproto { src, dst, msg } => agw.unproto(
            0,
            0xF0,
            &Call::from_str(&src)?,
            &Call::from_str(&dst)?,
            &msg.into_bytes(),
        )?,
        Command::Connect { src, dst } => {
            let src = &Call::from_str(&src)?;
            agw.register_callsign(0, 0xF0, src);
            let mut con = agw.connect(0, 0xF0, src, &Call::from_str(&dst)?, &[])?;
            eprintln!("Read: {:?}", ascii7_to_str(con.read()?));
            std::thread::sleep(std::time::Duration::from_millis(30000));
            con.write(b"BYE\r")?;
            loop {
                eprintln!("Read: {:?}", ascii7_to_str(con.read()?));
            }
            con.disconnect()?;
        }
    };
    Ok(())
}

fn ascii7_to_str(bytes: Vec<u8>) -> String {
    let mut s = String::new();
    for b in bytes.iter() {
        s.push((b & 0x7f) as char);
    }
    s
}

use agw::Call;
use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Cli {
    /// Subcommand
    command: String,

    #[clap(short)]
    verbose: Option<usize>,
}

fn main() -> Result<()> {
    let args = Cli::parse();
    stderrlog::new()
        .module(module_path!())
        .module("agw")
        .quiet(false)
        .verbosity(args.verbose.unwrap_or(0))
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    let mut agw = agw::AGW::new("127.0.0.1:8010")?;

    match args.command.as_str() {
        "version" => eprintln!("Version: {:?}", agw.version()?),
        "port_info" => eprintln!("{}", agw.port_info()?),
        "port_cap" => eprintln!("{}", agw.port_cap(0)?),
        "unproto" => agw.unproto(
            0,
            0xF0,
            &Call::from_str("M0THC-1")?,
            &Call::from_str("APZ001")?,
            b"hello world",
        )?,
        "connect" => {
            let mut con = agw.connect(
                0,
                0,
                &Call::from_str("M0THC-1")?,
                &Call::from_str("M0THC-2")?,
                &[],
            )?;
            con.disconnect()?;
        }
        _ => panic!("unknown command"),
    };
    Ok(())
}

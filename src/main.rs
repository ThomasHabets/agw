use anyhow::{Error, Result};
use clap::Parser;
use std::io::{Read, Write};
use std::net::TcpStream;

// Search for a pattern in a file and display the lines that contain it.
#[derive(Parser)]
struct Cli {
    /// Subcommand
    command: String,
}

fn port_info() -> Vec<u8> {
    Header::new(0, b'G', 0, None, None, 0)
        .serialize()
        .expect("can't happen: port_info serialization failed")
}

fn port_cap(port: u8) -> Vec<u8> {
    Header::new(port, b'g', 0, None, None, 0)
        .serialize()
        .expect("can't happen: port_cap serialization failed")
}

fn version_info() -> Vec<u8> {
    Header::new(0, b'R', 0, None, None, 0)
        .serialize()
        .expect("can't happen: version_info serialization failed")
}

fn connect(port: u8, pid: u8, src: &Call, dst: &Call) -> Result<Vec<u8>> {
    Header::new(port, b'C', pid, Some(src.clone()), Some(dst.clone()), 0).serialize()
}

fn unproto(port: u8, pid: u8, src: &Call, dst: &Call, data: &[u8]) -> Result<Vec<u8>> {
    let h = Header::new(
        port,
        b'M',
        pid,
        Some(src.clone()),
        Some(dst.clone()),
        data.len() as u32,
    )
    .serialize()?;
    Ok([h, data.to_vec()].concat())
}

#[derive(Clone)]
struct Call {
    bytes: [u8; 10],
}

fn parse_call(s: &str) -> Result<Call> {
    let bytes = s.as_bytes();
    if bytes.len() > 10 {
        return Err(Error::msg(format!(
            "callsign '{}' is longer than 10 characters",
            s
        )));
    }

    let mut arr = [0; 10];
    for (i, &item) in bytes.iter().enumerate() {
        arr[i] = item;
    }

    Ok(Call { bytes: arr })
}

fn parse_reply(header: &Header, data: &[u8]) {
    match header.data_kind {
        b'R' => {
            let major = u16::from_le_bytes(data[0..2].try_into().unwrap());
            let minor = u16::from_le_bytes(data[4..6].try_into().unwrap());
            eprintln!("Version {}.{}", major, minor);
        }
        b'G' => {
            eprintln!("Port info: {}", std::str::from_utf8(data).unwrap());
        }
        b'g' => {
            let rate = data[0];
            let traffic_level = data[1];
            let tx_delay = data[2];
            let tx_tail = data[3];
            let persist = data[4];
            let slot_time = data[5];
            let max_frame = data[6];
            let active_connections = data[7];
            let bytes_per_2min = u32::from_le_bytes(data[8..12].try_into().unwrap());
            eprintln!(
                "Port caps:
  rate={rate}
  traffic={traffic_level}
  txdelay={tx_delay}
  txtail={tx_tail}
  persist={persist}
  slot_time={slot_time}
  max_frame={max_frame}
  active_connections={active_connections}
  bytes_per_2min={bytes_per_2min}"
            );
        }
        k => {
            eprintln!("Unknown kind {}", k);
            let mut stdout = std::io::stdout();
            stdout.write(data).unwrap();
            stdout.flush().unwrap();
        }
    };
}

struct Header {
    port: u8,
    pid: u8,
    data_kind: u8,
    data_len: u32,
    src: Option<Call>,
    dst: Option<Call>,
}
const HEADER_LEN: usize = 36;
impl Header {
    fn new(
        port: u8,
        data_kind: u8,
        pid: u8,
        src: Option<Call>,
        dst: Option<Call>,
        data_len: u32,
    ) -> Header {
        Header {
            port,
            data_kind,
            pid,
            data_len,
            src,
            dst,
        }
    }
    fn serialize(&self) -> Result<Vec<u8>> {
        let mut v = vec![0; HEADER_LEN];
        v[0] = self.port;
        v[4] = self.data_kind;
        v[6] = self.pid;

        if let Some(src) = &self.src {
            v.splice(8..18, src.bytes.iter().cloned());
        }
        if let Some(dst) = &self.dst {
            v.splice(18..28, dst.bytes.iter().cloned());
        }
        v.splice(28..32, u32::to_le_bytes(self.data_len));
        Ok(v)
    }
}

fn parse_header(header: &[u8]) -> Header {
    if header.len() != HEADER_LEN {
        panic!();
    }
    Header {
        port: header[0],
        data_kind: header[4],
        data_len: u32::from_le_bytes(header[28..32].try_into().unwrap()),
        src: None, // TODO
        dst: None, // TODO
        pid: 0,    // TODO
    }
}

fn main() -> Result<()> {
    let args = Cli::parse();

    let mut stream = TcpStream::connect("127.0.0.1:8010")?;

    let msg = match args.command.as_str() {
        "version" => version_info(),
        "port_info" => port_info(),
        "port_cap" => port_cap(0),
        "connect" => connect(0, 0, &parse_call("M0THC-1")?, &parse_call("M0THC-2")?)?,
        _ => panic!("unknown command"),
    };

    /*
    let src = make_call("M0THC-1").unwrap();
    let dst = make_call("APZ001").unwrap();
    let msg = unproto(0, 0xF0, &src, &dst, b":M6VMB-1  :helloworld{3");
    */
    stream.write(&msg).unwrap();
    //eprintln!("Sent command!, awaiting reply...");

    let mut header = [0 as u8; HEADER_LEN];
    stream.read_exact(&mut header)?;
    let header = parse_header(&header);

    if header.data_len > 0 {
        let mut payload = vec![0; header.data_len as usize];
        stream.read_exact(&mut payload)?;
        parse_reply(&header, &payload);
    }

    Ok(())
}

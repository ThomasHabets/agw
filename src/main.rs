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

#[derive(Clone, Debug)]
struct Call {
    bytes: [u8; 10],
}

impl Call {
    fn parse(bytes: &[u8]) -> Call {
        let mut arr = [0; 10];
        for (i, &item) in bytes.iter().enumerate() {
            arr[i] = item;
        }
        Call { bytes: arr }
    }
    fn from_str(s: &str) -> Result<Call> {
        if s.len() > 10 {
            return Err(Error::msg(format!(
                "callsign '{}' is longer than 10 characters",
                s
            )));
        }
        let mut arr = [0; 10];
        for (i, &item) in s.as_bytes().iter().enumerate() {
            arr[i] = item;
        }
        Ok(Call { bytes: arr })
    }

    fn is_empty(&self) -> bool {
        for b in self.bytes {
            if b != 0 {
                return false;
            }
        }
        true
    }
}

enum Reply {
    // TODO: should these actually pick up the header value subset,
    // too, when appropriate?
    Version((u16, u16)),              // R.
    CallsignRegistration(bool),       // X.
    PortInfo(String),                 // G. TODO: parse
    PortCaps(String),                 // g. TODO: parse
    FramesOutstandingPort(u32),       // y.
    FramesOutstandingConnection(u32), // Y.
    HeardStations(String),            // H. TODO: parse
    Connected(String),                // C.
    ConnectedData(Vec<u8>),           // D.
    MonitorConnected(Vec<u8>),        // I.
    MonitorSupervisory(Vec<u8>),      // S.
    Unproto(Vec<u8>),                 // U.
    ConnectedSent(Vec<u8>),           // T.
    Raw(Vec<u8>),                     // R.
    Unknown(Header, Vec<u8>),
}

impl Reply {
    fn description(&self) -> String {
        match self {
            Reply::ConnectedData(data) => format!("ConnectedData: {:?}", data),
            Reply::ConnectedSent(data) => format!("ConnectedSent: {:?}", data),
            Reply::Unproto(data) => format!("Received unproto: {:?}", data),
            Reply::PortInfo(s) => format!("Port info: {}", s),
            Reply::PortCaps(s) => format!("Port caps: {}", s),
            Reply::Connected(s) => format!("Connected: {}", s),
            Reply::Version((maj, min)) => format!("Version: {maj}.{min}"),
            Reply::Raw(data) => "Raw".to_string(),
            Reply::CallsignRegistration(success) => format!("Callsign registration: {success}"),
            Reply::FramesOutstandingPort(n) => format!("Frames outstanding port: {n}"),
            Reply::FramesOutstandingConnection(n) => format!("Frames outstanding connection: {n}"),
            Reply::MonitorConnected(_) => format!("Connected packet"),
            Reply::MonitorSupervisory(_) => format!("Supervisory packet"),
            Reply::HeardStations(s) => format!("Heard stations: {s}"),
            Reply::Unknown(h, data) => format!("Unknown reply: header={h:?} data={data:?}"),
        }
    }
}

fn parse_reply(header: &Header, data: &[u8]) -> Result<Reply> {
    // TODO: confirm data len, since most replies will have fixed size.
    Ok(match header.data_kind {
        b'R' => {
            let major = u16::from_le_bytes(
                data[0..2]
                    .try_into()
                    .expect("can't happen: two bytes can't be made into u16?"),
            );
            let minor = u16::from_le_bytes(
                data[4..6]
                    .try_into()
                    .expect("can't happen: two bytes can't be made into u16?"),
            );
            Reply::Version((major, minor))
        }
        b'X' => Reply::CallsignRegistration(data[0] == 1),
        b'C' => Reply::Connected(std::str::from_utf8(data)?.to_string()),
        b'D' => Reply::ConnectedData(data.to_vec()),
        b'T' => Reply::ConnectedSent(data.to_vec()),
        b'U' => Reply::Unproto(data.to_vec()),
        b'G' => Reply::PortInfo(std::str::from_utf8(data)?.to_string()),
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

            Reply::PortCaps(format![
                "rate={rate}
  traffic={traffic_level}
  txdelay={tx_delay}
  txtail={tx_tail}
  persist={persist}
  slot_time={slot_time}
  max_frame={max_frame}
  active_connections={active_connections}
  bytes_per_2min={bytes_per_2min}"
            ])
        }
        b'y' => Reply::FramesOutstandingPort(u32::from_le_bytes(data[0..4].try_into().unwrap())),
        b'Y' => {
            Reply::FramesOutstandingConnection(u32::from_le_bytes(data[0..4].try_into().unwrap()))
        }
        b'H' => Reply::HeardStations(std::str::from_utf8(data)?.to_string()),
        b'I' => Reply::MonitorConnected(data.to_vec()),
        b'S' => Reply::MonitorSupervisory(data.to_vec()),
        k => {
            eprintln!("Unknown kind {}", k);
            let mut stdout = std::io::stdout();
            stdout.write(data).unwrap();
            stdout.flush().unwrap();
            Reply::Unknown(header.clone(), data.to_vec())
        }
    })
}

#[derive(Clone, Debug)]
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

fn parse_header(header: &[u8; HEADER_LEN]) -> Header {
    let src = Call::parse(&header[8..18]);
    let src = if src.is_empty() { None } else { Some(src) };
    let dst = Call::parse(&header[18..28]);
    let dst = if dst.is_empty() { None } else { Some(dst) };
    Header {
        port: header[0],
        data_kind: header[4],
        pid: header[6],
        src: src,
        dst: dst,
        data_len: u32::from_le_bytes(header[28..32].try_into().unwrap()),
    }
}

fn main() -> Result<()> {
    let args = Cli::parse();

    let mut stream = TcpStream::connect("127.0.0.1:8010")?;

    let msg = match args.command.as_str() {
        "version" => version_info(),
        "port_info" => port_info(),
        "port_cap" => port_cap(0),
        "unproto" => unproto(
            0,
            0xF0,
            &Call::from_str("M0THC-1")?,
            &Call::from_str("APZ001")?,
            b"hello world",
        )?,
        "connect" => connect(
            0,
            0,
            &Call::from_str("M0THC-1")?,
            &Call::from_str("M0THC-2")?,
        )?,
        _ => panic!("unknown command"),
    };

    stream.write(&msg)?;

    // Read reply
    let mut header = [0 as u8; HEADER_LEN];
    stream.read_exact(&mut header)?;
    let header = parse_header(&header);
    if header.data_len > 0 {
        let mut payload = vec![0; header.data_len as usize];
        stream.read_exact(&mut payload)?;
        match parse_reply(&header, &payload) {
            Ok(reply) => eprintln!("Reply: {}", reply.description()),
            Err(e) => eprintln!("Error parsing reply: {:?}", e),
        }
    }

    Ok(())
}

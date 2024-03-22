use std::io::{Read, Write};
use std::net::TcpStream;

fn port_info() -> Vec<u8> {
    let mut v = vec![0; 36];
    v[4] = b'G';
    v
}

fn port_cap(n: u8) -> Vec<u8> {
    let mut v = vec![0; 36];
    v[0] = n;
    v[4] = b'g';
    v
}

fn version_info() -> Vec<u8> {
    let mut v = vec![0; 36];
    v[4] = b'R';
    v
}

struct Call {
    bytes: [u8; 10],
}

fn make_call(s: &str) -> Result<Call, &'static str> {
    let bytes = s.as_bytes();
    if bytes.len() > 10 {
        return Err("callsign '{}' is longer than 10 characters");
    }

    let mut arr = [0; 10];
    for (i, &item) in bytes.iter().enumerate() {
        arr[i] = item;
    }

    Ok(Call { bytes: arr })
}

fn unproto(port: u8, pid: u8, src: &Call, dst: &Call, data: &[u8]) -> Vec<u8> {
    let mut v = vec![0; data.len() + 36];
    v[0] = port;
    v[4] = b'M';
    v[6] = pid;

    v.splice(8..18, src.bytes.iter().cloned());
    v.splice(18..28, dst.bytes.iter().cloned());
    v.splice(28..32, u32::to_le_bytes(data.len() as u32));
    v.splice(36.., data.iter().cloned());

    let mut stdout = std::io::stdout();
    stdout.write(&v);
    stdout.flush().unwrap();
    eprintln!("Packet len: {}", v.len());
    v
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
    data_kind: u8,
    data_len: u32,
}

fn parse_header(header: &[u8]) -> Header {
    if header.len() != 36 {
        panic!();
    }
    Header {
        port: header[0],
        data_kind: header[4],
        data_len: u32::from_le_bytes(header[28..32].try_into().unwrap()),
    }
}

fn main() -> std::io::Result<()> {
    let mut stream = TcpStream::connect("127.0.0.1:8010")?;

    //let msg = version_info();
    //let msg = port_info();
    let msg = port_cap(0);

    /*
    let src = make_call("M0THC-1").unwrap();
    let dst = make_call("APZ001").unwrap();
    let msg = unproto(0, 0xF0, &src, &dst, b":M6VMB-1  :helloworld{3");
    */
    stream.write(&msg).unwrap();
    //eprintln!("Sent command!, awaiting reply...");

    let mut header = [0 as u8; 36];
    stream.read_exact(&mut header)?;
    let header = parse_header(&header);

    if header.data_len > 0 {
        let mut payload = vec![0; header.data_len as usize];
        stream.read_exact(&mut payload)?;
        parse_reply(&header, &payload);
    }

    Ok(())
}

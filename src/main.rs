use core::str::from_utf8;
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

fn main() -> std::io::Result<()> {
    let mut stream = TcpStream::connect("127.0.0.1:8010")?;

    //let msg = version_info();
    //let msg = port_info();
    //let msg = port_cap(0);
    let src = make_call("M0THC-1").unwrap();
    let dst = make_call("APZ001").unwrap();

    let msg = unproto(0, 0xF0, &src, &dst, b":M6VMB-1  :helloworld{3");
    stream.write(&msg).unwrap();
    eprintln!("Sent command!, awaiting reply...");

    let mut data = [0 as u8; 1024];
    let n = stream.read(&mut data)?;

    if false {
        let mut stdout = std::io::stdout();
        stdout.write(&data[..n])?;
        stdout.flush()?;
    }
    Ok(())
}

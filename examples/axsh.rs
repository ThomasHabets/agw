use anyhow::Result;

#[cfg(feature = "native")]
fn main2() -> Result<()> {
    use agw::native::{parse_call, NativeStream, Stream};
    use std::io::BufRead;
    let stream: &mut dyn Stream = &mut NativeStream::connect(
        &parse_call("M0THC-1")?, // Mycall.
        &parse_call("M0THC-1")?, // Radio call.
        &parse_call("M0THC-2")?, // Remote end.
        &[],
    )
    .expect("connect()");

    for line in std::io::stdin().lock().lines() {
        stream.write(line?.as_bytes()).expect("write");
        loop {
            let mut buf = [0u8; 1024];
            let n = match stream.read(&mut buf) {
                Ok(n) => n,
                Err(err) => {
                    eprintln!("Reading: {err}");
                    break;
                }
            };
            let buf = &buf[..n];
            // TODO: this could be partial unicode. Handle that.
            let s = String::from_utf8(buf.to_vec())?;
            print!("{s}");
        }
        println!("end!");
    }
    Ok(())
}

fn main() -> Result<()> {
    #[cfg(feature = "native")]
    {
        main2()
    }
    #[cfg(not(feature = "native"))]
    {
        Ok(())
    }
}

use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::Result;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use log::{debug, trace};

use crate::Packet;

/// AGW proxy stream.
pub struct Proxy {
    up: ConnectionV2,
    down: ConnectionV2,
}

impl Proxy {
    pub fn new(down: TcpStream) -> Result<Self> {
        let addr = "127.0.0.1:8010";
        let up = TcpStream::connect(addr)?;
        Ok(Self {
            up: ConnectionV2::new(up)?,
            down: ConnectionV2::new(down)?,
        })
    }
    pub fn run(
        &mut self,
        cb_up: &dyn Fn(Packet) -> Packet,
        cb_down: &dyn Fn(Packet) -> Packet,
    ) -> Result<()> {
        eprintln!("Running proxy");
        self.up.send(Packet::VersionQuery)?;
        loop {
            select! {
                recv(self.down.rx) -> packet => {
                    debug!("Got {packet:?} from downstream");
            let packet = packet?;
                    let packet = cb_down(packet);
                    debug!("… Transformed into {packet:?}");
                },
                recv(self.up.rx) -> packet => {
                    debug!("Got {packet:?} from up");
                    let packet = cb_up(packet?);
                    debug!("Transformed into {packet:?}");
                },
                };
        }
    }
}

struct ConnectionV2 {
    rx: Receiver<Packet>,
    tx: Sender<Packet>,
    rxthread: Option<std::thread::JoinHandle<Result<()>>>,
    txthread: Option<std::thread::JoinHandle<Result<()>>>,
}

impl Drop for ConnectionV2 {
    fn drop(&mut self) {
        debug!("Awaiting proxy thread shutdown");
        let _ = self.txthread.take().unwrap().join();
        let _ = self.rxthread.take().unwrap().join();
    }
}
impl ConnectionV2 {
    fn rx_loop(mut rstream: TcpStream, tx: Sender<Packet>) -> Result<()> {
        loop {
            let mut header = [0_u8; crate::HEADER_LEN];
            rstream.read_exact(&mut header)?;

            let header = crate::parse_header(&header)?;
            let payload = if header.data_len() > 0 {
                let mut payload = vec![0; header.data_len() as usize];
                rstream.read_exact(&mut payload)?;
                payload
            } else {
                Vec::new()
            };
            //let reply = parse_reply(&header, &payload)?;
            //tx.send((header, reply))?;
            let packet = Packet::parse(&header, &payload)?;
            trace!("ConnectionV2 rx_loop: {packet:?}");
            tx.send(packet)?;
        }
    }
    fn new(rstream: TcpStream) -> Result<Self> {
        let mut wstream = rstream.try_clone()?;
        let (rxtx, rxrx) = unbounded::<Packet>();
        let rxthread = std::thread::spawn(move || -> Result<()> { Self::rx_loop(rstream, rxtx) });
        let (txtx, txrx) = unbounded::<Packet>();
        let txthread = std::thread::spawn(move || {
            for packet in txrx {
                let bytes = packet.serialize();
                let _ = wstream.write(&bytes)?;
                eprintln!("Send: {bytes:?}");
            }
            Ok(())
        });
        Ok(Self {
            rxthread: Some(rxthread),
            txthread: Some(txthread),
            rx: rxrx,
            tx: txtx,
        })
    }
    fn send(&self, packet: Packet) -> Result<(), crossbeam_channel::SendError<Packet>> {
        self.tx.send(packet)
    }
}

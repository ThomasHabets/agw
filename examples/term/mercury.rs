//! Bridge Mercury's asynchronous control/data sockets to the terminal threads.

use std::{sync::mpsc, time::Duration};

use anyhow::{bail, Context, Result};
use mercury_hf::{Callsign, ClientConfig, ControlClient, Event, EventReceiver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Connection {
    incoming: mpsc::Receiver<Result<Vec<u8>>>,
    writer: Writer,
    status: String,
}

#[derive(Clone)]
pub struct Writer(tokio::sync::mpsc::UnboundedSender<Command>);

enum Command {
    Data(Vec<u8>),
    Disconnect,
}

impl Connection {
    pub fn connect(config: ClientConfig, src: Callsign, dst: Callsign) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let (incoming_tx, incoming) = mpsc::channel();
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("term-mercury".into())
            .spawn(move || {
                let result =
                    runtime.block_on(run(config, src, dst, &ready_tx, &incoming_tx, command_rx));
                if let Err(error) = result {
                    // Before setup completes the error belongs to connect();
                    // afterwards it belongs to read(). Only one is consumed.
                    let _ = ready_tx.send(Err(anyhow::anyhow!("{error:#}")));
                    let _ = incoming_tx.send(Err(error));
                }
            })?;
        let status = ready_rx
            .recv()
            .context("Mercury worker stopped during setup")??;
        Ok(Self {
            incoming,
            writer: Writer(commands),
            status,
        })
    }

    pub fn connect_string(&self) -> &str {
        &self.status
    }

    pub fn read(&self) -> Result<Vec<u8>> {
        self.incoming.recv().context("Mercury connection closed")?
    }

    pub fn writer(&self) -> Writer {
        self.writer.clone()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.writer.disconnect();
    }
}

impl Writer {
    pub fn send(&self, data: Vec<u8>) -> Result<()> {
        self.0
            .send(Command::Data(data))
            .context("Mercury connection closed")
    }

    pub fn disconnect(&self) -> Result<()> {
        self.0
            .send(Command::Disconnect)
            .context("Mercury connection closed")
    }
}

async fn wait_connected(
    events: &mut EventReceiver,
    src: &Callsign,
    dst: &Callsign,
) -> Result<String> {
    loop {
        match events.recv().await? {
            Event::Connected(session) => {
                if session.source != *src || session.destination != *dst {
                    bail!("Mercury connected an unexpected station: {session:?}");
                }
                return Ok(format!("Connected with station {dst} via Mercury"));
            }
            Event::Disconnected => bail!("Mercury connection failed before connecting"),
            Event::Closed(error) => return Err(error.into()),
            event => log::debug!("Mercury setup: {event:?}"),
        }
    }
}

async fn run(
    config: ClientConfig,
    src: Callsign,
    dst: Callsign,
    ready: &mpsc::Sender<Result<String>>,
    incoming: &mpsc::Sender<Result<Vec<u8>>>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
) -> Result<()> {
    let (client, mut events) = ControlClient::connect(config).await?;
    let setup = async {
        // Open data before calling so the remote's initial banner is buffered.
        let data = client.open_data_stream().await?;
        client.mycall(src.clone(), vec![]).await?;
        client.connect_arq(src.clone(), dst.clone()).await?;
        let status = tokio::time::timeout(CONNECT_TIMEOUT, wait_connected(&mut events, &src, &dst))
            .await
            .context("Mercury connection attempt timed out")??;
        Ok::<_, anyhow::Error>((data, status))
    }
    .await;
    let (data, status) = match setup {
        Ok(connected) => connected,
        Err(error) => {
            let _ = client.abort_arq().await;
            client.close().await;
            return Err(error);
        }
    };
    ready.send(Ok(status))?;
    let (mut reader, mut writer) = data.into_split();
    let write_client = client.clone();
    // Keep writes in a separate task: a control event must neither cancel a
    // partial write_all nor wait for a stalled data socket to become writable.
    let mut outgoing_task = tokio::spawn(async move {
        while let Some(command) = commands.recv().await {
            match command {
                Command::Data(data) => writer.write_all(&data).await?,
                Command::Disconnect => {
                    write_client.disconnect_arq().await?;
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
        write_client.disconnect_arq().await?;
        Ok(())
    });
    let result = async {
        let mut buffer = [0; 4096];
        loop {
            tokio::select! {
                event = events.recv() => match event? {
                    Event::Disconnected => return Ok(()),
                    Event::Closed(error) => return Err(error.into()),
                    event => log::debug!("Mercury: {event:?}"),
                },
                read = reader.read(&mut buffer) => {
                    let len = read?;
                    if len == 0 { bail!("Mercury data connection closed"); }
                    incoming.send(Ok(buffer[..len].to_vec()))?;
                },
                result = &mut outgoing_task => return result?,
            }
        }
    }
    .await;
    outgoing_task.abort();
    if result.is_err() && client.status().transport_open {
        let _ = client.abort_arq().await;
    }
    client.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn command(control: &mut BufReader<TcpStream>, expected: &[u8]) {
        let mut line = Vec::new();
        control.read_until(b'\r', &mut line).unwrap();
        assert_eq!(line, expected);
    }

    fn mock(
        peer: impl FnOnce(BufReader<TcpStream>, TcpStream) + Send + 'static,
    ) -> (ClientConfig, std::thread::JoinHandle<()>) {
        let control = TcpListener::bind("127.0.0.1:0").unwrap();
        let data = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = ClientConfig::new("127.0.0.1");
        config.control_port = control.local_addr().unwrap().port();
        config.data_port = data.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            let (control, _) = control.accept().unwrap();
            control.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
            let (data, _) = data.accept().unwrap();
            data.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
            let mut control = BufReader::new(control);
            command(&mut control, b"MYCALL LOCAL\r");
            control.get_mut().write_all(b"OK\r").unwrap();
            command(&mut control, b"CONNECT LOCAL REMOTE\r");
            peer(control, data);
        });
        (config, thread)
    }

    fn connect(config: ClientConfig) -> Result<Connection> {
        Connection::connect(config, "LOCAL".parse().unwrap(), "REMOTE".parse().unwrap())
    }

    #[test]
    fn relays_binary_data_and_disconnects_on_request() {
        let (config, peer) = mock(|mut control, mut data| {
            // Events may arrive even before the CONNECT command acknowledgment.
            control
                .get_mut()
                .write_all(b"CONNECTED LOCAL REMOTE 2300\rOK\r")
                .unwrap();
            data.write_all(b"\0\xff\x18hello").unwrap();
            let mut received = [0; 4];
            data.read_exact(&mut received).unwrap();
            assert_eq!(&received, b"\xff\0\x18B");
            command(&mut control, b"DISCONNECT\r");
            control.get_mut().write_all(b"OK\rDISCONNECTED\r").unwrap();
        });
        let connection = connect(config).unwrap();
        assert_eq!(
            connection.connect_string(),
            "Connected with station REMOTE via Mercury"
        );
        let mut bytes = Vec::new();
        while bytes.len() < 8 {
            bytes.extend(
                connection
                    .incoming
                    .recv_timeout(TEST_TIMEOUT)
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(bytes, b"\0\xff\x18hello");
        let writer = connection.writer();
        writer.send(b"\xff\0\x18B".to_vec()).unwrap();
        writer.disconnect().unwrap();
        peer.join().unwrap();
    }

    #[test]
    fn disconnected_event_ends_read_even_with_data_socket_open() {
        let (release, held) = mpsc::channel();
        let (config, peer) = mock(move |mut control, _data| {
            control
                .get_mut()
                .write_all(b"OK\rCONNECTED LOCAL REMOTE 2300\rDISCONNECTED\r")
                .unwrap();
            held.recv_timeout(TEST_TIMEOUT).unwrap();
        });
        let connection = connect(config).unwrap();
        assert!(matches!(
            connection.incoming.recv_timeout(TEST_TIMEOUT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(connection.read().is_err());
        release.send(()).unwrap();
        peer.join().unwrap();
    }

    #[test]
    fn failed_call_returns_error_after_acknowledgment() {
        let (config, peer) = mock(|mut control, _data| {
            control.get_mut().write_all(b"OK\rDISCONNECTED\r").unwrap();
            command(&mut control, b"ABORT\r");
            control.get_mut().write_all(b"OK\r").unwrap();
        });
        let Err(error) = connect(config) else {
            panic!("failed call was accepted")
        };
        assert!(error.to_string().contains("failed before connecting"));
        peer.join().unwrap();
    }

    #[test]
    fn control_loss_ends_read_even_with_data_socket_open() {
        let (release, held) = mpsc::channel();
        let (config, peer) = mock(move |mut control, _data| {
            control
                .get_mut()
                .write_all(b"OK\rCONNECTED LOCAL REMOTE 2300\r")
                .unwrap();
            drop(control);
            held.recv_timeout(TEST_TIMEOUT).unwrap();
        });
        let connection = connect(config).unwrap();
        assert!(connection
            .incoming
            .recv_timeout(TEST_TIMEOUT)
            .unwrap()
            .is_err());
        release.send(()).unwrap();
        peer.join().unwrap();
    }
}

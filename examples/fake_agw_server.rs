//! AGW server that just makes stuff up.
use anyhow::Result;
use clap::Parser;
use log::{info, warn};
use tokio::net::{TcpListener, TcpStream};

use agw::r#async::AGWServer;
use agw::{Baud, Call, Packet, Pid, Port, PortCaps, PortInfo, PortsInfo};

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// AGW server that just makes stuff up.
#[derive(Parser, Debug)]
#[clap(version)]
struct Opt {
    /// Log level for stderr diagnostics.
    #[arg(short = 'v', long = "log-level", value_enum, default_value = "info")]
    log_level: LogLevel,

    #[clap(short, long, default_value = "[::1]:8110")]
    listen: String,
}

fn fake_port_info_reply() -> Packet {
    Packet::PortInfoReply(PortsInfo {
        count: 1,
        ports: vec![PortInfo {
            port: Port(1),
            descr: "Fake AGW port".to_string(),
        }],
    })
}

fn fake_port_cap_reply(port: Port) -> Packet {
    Packet::PortCapReply {
        port,
        caps: PortCaps {
            rate: Baud::B1200,
            traffic_level: None,
            tx_delay: 30,
            tx_tail: 10,
            persist: 63,
            slot_time: 10,
            max_frame: 4,
            active_connections: 0,
            bytes_per_2min: 0,
        },
    }
}

async fn send_connect_reply(
    server: &mut AGWServer,
    port: Port,
    pid: Pid,
    src: &Call,
    dst: &Call,
) -> Result<()> {
    // Replies are from the AGW server back toward the client, so src/dst are
    // reversed compared to the client's original connect request.
    server
        .send(&Packet::ConnectionEstablished {
            port,
            pid,
            src: dst.clone(),
            dst: src.clone(),
        })
        .await?;
    Ok(())
}

async fn send_disconnect_reply(
    server: &mut AGWServer,
    port: Port,
    pid: Pid,
    src: &Call,
    dst: &Call,
) -> Result<()> {
    server
        .send(&Packet::Disconnect {
            port,
            pid,
            src: dst.clone(),
            dst: src.clone(),
        })
        .await?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_client(stream: TcpStream) -> Result<()> {
    let mut server = AGWServer::new(stream);
    let peer = server
        .peer_addr()
        .map_or_else(|e| format!("<unknown peer: {e}>"), |addr| addr.to_string());

    info!("{peer}: connected");
    loop {
        let packet = match server.recv().await {
            Ok(packet) => packet,
            Err(agw::Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                info!("{peer}: disconnected");
                return Ok(());
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        info!("{peer}: {packet:?}");
        match packet {
            Packet::VersionQuery => {
                server
                    .send(&Packet::VersionReply {
                        major: 2005,
                        minor: 127,
                    })
                    .await?;
            }
            Packet::FramesOutstandingPortQuery(port) => {
                server
                    .send(&Packet::FramesOutstandingPortReply(port, 0))
                    .await?;
            }
            Packet::PortInfoQuery => {
                server.send(&fake_port_info_reply()).await?;
            }
            Packet::PortCapQuery(port) => {
                server.send(&fake_port_cap_reply(port)).await?;
            }
            Packet::CallsignHeardQuery(port) => {
                server
                    .send(&Packet::CallsignHeardReply {
                        port,
                        data: vec![0],
                    })
                    .await?;
            }
            Packet::RegisterCallsign(port, call) => {
                server
                    .send(&Packet::RegisterCallsignReply {
                        port,
                        call,
                        success: true,
                    })
                    .await?;
            }
            Packet::Connect {
                port,
                pid,
                src,
                dst,
            }
            | Packet::ConnectVia {
                port,
                pid,
                src,
                dst,
                via: _,
            } => {
                send_connect_reply(&mut server, port, pid, &src, &dst).await?;
            }
            Packet::Disconnect {
                port,
                pid,
                src,
                dst,
            } => {
                send_disconnect_reply(&mut server, port, pid, &src, &dst).await?;
            }
            Packet::Data {
                port,
                pid,
                src,
                dst,
                data,
            } => {
                server
                    .send(&Packet::Data {
                        port,
                        pid,
                        src: dst,
                        dst: src,
                        data,
                    })
                    .await?;
            }
            Packet::Unproto { .. }
            | Packet::IncomingConnect { .. }
            | Packet::ConnectionEstablished { .. }
            | Packet::FramesOutstandingPortReply(_, _)
            | Packet::VersionReply { .. }
            | Packet::RegisterCallsignReply { .. }
            | Packet::PortCapReply { .. }
            | Packet::CallsignHeardReply { .. }
            | Packet::PortInfoReply(_) => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();
    stderrlog::new()
        .module(module_path!())
        .module("agw")
        .quiet(false)
        .verbosity(opt.log_level as usize)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();
    info!("Starting up");

    let listener = TcpListener::bind(&opt.listen).await?;
    info!("listening on {}", opt.listen);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream).await {
                warn!("Client task failed: {e:?}");
            }
        });
    }
}

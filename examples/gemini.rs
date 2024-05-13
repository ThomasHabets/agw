use anyhow::Result;
use clap::Parser;
use log::{debug, error, info, warn};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs::File;
use std::io::BufReader;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::{rustls, TlsAcceptor};

#[derive(Parser, Debug)]
struct Opts {
    #[clap(short, default_value = "localhost:1965")]
    addr: String,

    #[clap(short)]
    cert: std::path::PathBuf,

    #[clap(short)]
    key: std::path::PathBuf,

    #[clap(short, default_value = "10")]
    verbose: usize,
}

async fn listen_first(addrs: &str) -> Result<TcpListener> {
    for addr in addrs.to_socket_addrs()? {
        match TcpListener::bind(&addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) => warn!("Failed to bind to {addr:?}: {e:?}"),
        }
    }
    todo!()
}

fn load_certs(path: &std::path::Path) -> std::io::Result<Vec<CertificateDer<'static>>> {
    rustls_pemfile::certs(&mut BufReader::new(File::open(path)?)).collect()
}

fn load_key(path: &std::path::Path) -> std::io::Result<PrivateKeyDer<'static>> {
    debug!("Loading key {path:?}");
    Ok(rustls_pemfile::private_key(&mut BufReader::new(File::open(path)?))?.unwrap())
}

async fn run_connection(conn: tokio::net::TcpStream, acceptor: TlsAcceptor) -> Result<()> {
    // TLS handshake.
    let mut stream = acceptor.accept(conn).await?;

    // Read request.
    let mut req = vec![];
    loop {
        let mut buf = [0_u8; 1024];
        let n = stream.read(&mut buf).await?;
        req.extend_from_slice(&buf[0..n]);
        let len = req.len();
        if req[len - 1] == 10_u8 {
            req.pop();
            if len > 2 && req[len - 2] == 13_u8 {
                req.pop();
            }
            break;
        }
    }
    let req = String::from_utf8(req)?;
    info!("Got req: {req:?}");

    // TODO: Proxy the request through AGW.

    // Write reply.
    stream.write(b"20 text/gemini\r\nHello world").await?;
    debug!("Write finished");
    Ok(())
}

async fn start_connection(
    (conn, addr): (tokio::net::TcpStream, std::net::SocketAddr),
    acceptor: TlsAcceptor,
) {
    info!("Got connection from {addr:?}");

    tokio::spawn(async move {
        if let Err(e) = run_connection(conn, acceptor).await {
            error!("Error in connection: {e:?}");
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opts::parse();
    stderrlog::new()
        .module(module_path!())
        .module("agw")
        .quiet(false)
        .verbosity(opt.verbose)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    // Load stuff before trying to bind.
    let certs = load_certs(&opt.cert)?;
    let key = load_key(&opt.key)?;

    // Bind.
    let listener = listen_first(&opt.addr).await?;

    // TLS server config.
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    let acceptor = TlsAcceptor::from(Arc::new(config));
    loop {
        start_connection(listener.accept().await?, acceptor.clone()).await;
    }
}

mod config;
mod proxy;
mod translate;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

const MAX_CONNECTIONS: usize = 4096;

fn main() {
    if let Err(e) = run() {
        eprintln!("eek: {e}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("EEK_CONFIG").ok())
        .ok_or("usage: eek <config.toml> (or set EEK_CONFIG)")?;
    let cfg = config::load(&path)?;

    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(Duration::from_secs(10)));
    let tls = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())?
        .https_only()
        .enable_http1()
        .wrap_connector(http);
    let client = Client::builder(TokioExecutor::new()).build(tls);

    let gw = Arc::new(proxy::Gateway::new(cfg, client)?);
    let listener = TcpListener::bind(gw.listen).await?;
    if !gw.listen.ip().is_loopback() {
        eprintln!("WARNING: listening on non-loopback {}", gw.listen);
    }
    eprintln!(
        "listening on http://{} ({} providers)",
        gw.listen,
        gw.routes.len()
    );

    let mut server = http1::Builder::new();
    server
        .timer(TokioTimer::new())
        .header_read_timeout(gw.header_read_timeout);
    let graceful = GracefulShutdown::new();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("accept: {e}");
                        continue;
                    }
                };
                if graceful.count() >= MAX_CONNECTIONS {
                    eprintln!("connection limit reached, dropping");
                    let _ = stream.try_write(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    );
                    continue;
                }
                let _ = stream.set_nodelay(true);
                let gw = gw.clone();
                let svc = service_fn(move |req| {
                    let gw = gw.clone();
                    async move { Ok::<_, Infallible>(proxy::handle(&gw, req).await) }
                });
                let conn = server.serve_connection(TokioIo::new(stream), svc);
                let conn = graceful.watch(conn);
                tokio::spawn(async move {
                    let _ = conn.await;
                });
            }
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
        }
    }
    eprintln!("draining connections");
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
    }
    Ok(())
}

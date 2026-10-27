//! Byte-stream connectors. Production uses TCP; tests use in-memory duplex
//! streams served by the K4 simulator, so the same codecs run either way.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> AsyncStream for T {}

pub type BoxStream = Box<dyn AsyncStream>;
pub type ConnectFuture<'a> = Pin<Box<dyn Future<Output = std::io::Result<BoxStream>> + Send + 'a>>;

pub trait Connector: Send + Sync + std::fmt::Debug {
    fn connect(&self) -> ConnectFuture<'_>;
    /// Human-readable target for logs (not stored in measurement shards).
    fn describe(&self) -> String;
}

#[derive(Debug, Clone)]
pub struct TcpConnector {
    pub host: String,
    pub port: u16,
    pub timeout: Duration,
}

impl Connector for TcpConnector {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(async move {
            let addr = format!("{}:{}", self.host, self.port);
            let stream = tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(&addr))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, format!("connect to {addr} timed out"))
                })??;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream) as BoxStream)
        })
    }

    fn describe(&self) -> String {
        format!("tcp://{}:{}", self.host, self.port)
    }
}

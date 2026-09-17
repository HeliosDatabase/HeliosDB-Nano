//! Shared `Plain`/`Tls` connection wrapper used by both the PostgreSQL and
//! MySQL wire listeners.
//!
//! Moved out of `postgres::ssl` so the MySQL TLS support (`protocol::mysql::ssl`
//! / `protocol::mysql::server`) can reuse the identical dispatch shape instead
//! of duplicating it. Re-exported from `postgres::ssl` for source
//! compatibility with existing callers.

use tokio::io::{AsyncRead, AsyncWrite};

/// Connection wrapper that can be either plain or TLS-encrypted.
pub enum SecureConnection<S> {
    /// Plain TCP connection
    Plain(S),
    /// TLS-encrypted connection
    Tls(tokio_rustls::server::TlsStream<S>),
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for SecureConnection<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for SecureConnection<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

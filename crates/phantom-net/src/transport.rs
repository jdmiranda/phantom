//! WebSocket transport layer.
//!
//! Wraps `tokio-tungstenite` to provide a simple framed send/receive
//! abstraction over a WebSocket connection.
//!
//! # Upgrade path
//! QUIC (via `quinn`) is the intended long-term transport once relay servers
//! support it.  Switching transports requires only swapping this module out —
//! [`RelayClient`](crate::client::RelayClient) depends on [`WsTransport`]
//! through async trait calls, not concrete types.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderName, HeaderValue},
        Message,
    },
    MaybeTlsStream, WebSocketStream,
};

// ---------------------------------------------------------------------------
// WsTransport
// ---------------------------------------------------------------------------

/// A connected WebSocket session.
///
/// Wraps the split send/recv halves internally so callers do not need to
/// juggle `SplitSink`/`SplitStream` generics.
pub struct WsTransport {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    connected: bool,
}

impl WsTransport {
    /// Open a WebSocket connection to `url` (e.g. `"wss://relay.example.com"`).
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_headers(url, &[]).await
    }

    /// Open a WebSocket connection to `url` with extra HTTP headers attached
    /// to the upgrade request.
    ///
    /// Each `(name, value)` pair is inserted into the upgrade request's
    /// header map verbatim.  Use this to send `Authorization: Bearer <jwt>`
    /// when dialing a hub that requires device-token auth.
    ///
    /// When `headers` is empty this is equivalent to [`Self::connect`].
    pub async fn connect_with_headers(
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Self> {
        let (ws, _response) = if headers.is_empty() {
            connect_async(url)
                .await
                .with_context(|| format!("WebSocket connect failed: {url}"))?
        } else {
            let mut request = url
                .into_client_request()
                .with_context(|| format!("invalid WS URL: {url}"))?;
            let header_map = request.headers_mut();
            for (name, value) in headers {
                let header_name = HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid header name: {name}"))?;
                let header_value = HeaderValue::from_str(value)
                    .with_context(|| format!("invalid header value for {name}"))?;
                header_map.insert(header_name, header_value);
            }
            connect_async(request)
                .await
                .with_context(|| format!("WebSocket connect failed: {url}"))?
        };
        Ok(Self {
            ws,
            connected: true,
        })
    }

    /// Send a binary frame.
    pub async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.ws
            .send(Message::Binary(bytes.into()))
            .await
            .context("WebSocket send failed")?;
        Ok(())
    }

    /// Receive the next binary frame.
    ///
    /// Skips non-binary frames (ping/pong/text) transparently.
    /// Returns `None` when the connection is cleanly closed.
    pub async fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.ws.next().await {
                None => {
                    self.connected = false;
                    return Ok(None);
                }
                Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes.to_vec())),
                Some(Ok(Message::Close(_))) => {
                    self.connected = false;
                    return Ok(None);
                }
                Some(Ok(Message::Ping(data))) => {
                    // Respond to pings to keep the connection alive.
                    let _ = self.ws.send(Message::Pong(data)).await;
                }
                Some(Ok(_)) => {
                    // Text / Pong / Frame — not used; skip.
                }
                Some(Err(e)) => {
                    self.connected = false;
                    return Err(e).context("WebSocket recv error");
                }
            }
        }
    }

    /// Returns `true` while the underlying WebSocket is believed to be open.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Initiate a graceful close handshake.
    pub async fn close(&mut self) -> Result<()> {
        self.ws
            .close(None)
            .await
            .context("WebSocket close failed")?;
        self.connected = false;
        Ok(())
    }
}

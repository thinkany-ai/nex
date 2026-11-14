//! Byte/message plumbing between WebSockets and TCP sockets.

use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

/// Carries a TCP stream over a WebSocket as binary frames, both directions,
/// until both sides are done.
pub async fn ws_tcp<S>(ws: WebSocketStream<S>, tcp: TcpStream)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();

    let up = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    };

    let down = async {
        while let Some(msg) = ws_rx.next().await {
            let written = match msg {
                Ok(Message::Binary(bytes)) => tcp_tx.write_all(&bytes).await,
                Ok(Message::Text(text)) => tcp_tx.write_all(text.as_bytes()).await,
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => Ok(()),
            };
            if written.is_err() {
                break;
            }
        }
        let _ = tcp_tx.shutdown().await;
    };

    tokio::join!(up, down);
}

/// Passes WebSocket messages between two sockets unchanged until either closes.
pub async fn ws_ws<A, B>(a: WebSocketStream<A>, b: WebSocketStream<B>)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();

    let a_to_b = async {
        while let Some(Ok(msg)) = a_rx.next().await {
            match msg {
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                data => {
                    if b_tx.send(data).await.is_err() {
                        break;
                    }
                }
            }
        }
    };
    let b_to_a = async {
        while let Some(Ok(msg)) = b_rx.next().await {
            match msg {
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                data => {
                    if a_tx.send(data).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = a_to_b => {}
        _ = b_to_a => {}
    }
    let _ = a_tx.send(Message::Close(None)).await;
    let _ = b_tx.send(Message::Close(None)).await;
}

/// Sends every text or binary message straight back until the peer closes.
pub async fn echo<S>(ws: WebSocketStream<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut tx, mut rx) = ws.split();
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(_) | Message::Binary(_) => {
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    let _ = tx.send(Message::Close(None)).await;
}

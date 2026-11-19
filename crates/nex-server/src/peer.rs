//! For a remote owner, forward the HTTP upgrade and tunnel the upgraded
//! connection to the owner's private listener. It runs the usual WebSocket
//! handlers, so handshakes, close codes, and SDK behavior are preserved.

use crate::{
    api::{ApiError, AuthDevice},
    relay::ConnectQuery,
    util::hash_secret,
    AppState,
};
use axum::{
    body::Body,
    extract::{FromRequestParts, Query, Request, State},
    http::{header, Method, StatusCode, Version},
    middleware::Next,
    response::Response,
};
use hyper_util::rt::TokioIo;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

pub(crate) async fn route(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.config.peer_addr.is_none()
        || request.method() != Method::GET
        || request.version() != Version::HTTP_11
        || !request
            .headers()
            .get(header::UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
        || !request
            .headers()
            .get_all(header::CONNECTION)
            .iter()
            .any(|value| {
                value.to_str().is_ok_and(|value| {
                    value
                        .split(',')
                        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
                })
            })
    {
        return Ok(next.run(request).await);
    }
    let (mut parts, body) = request.into_parts();
    let peer = match parts.uri.path() {
        "/v1/connect" => {
            let Query(query) = Query::<ConnectQuery>::from_request_parts(&mut parts, &state)
                .await
                .map_err(|err| ApiError::bad_request(err.body_text()))?;
            state.peer_for_ticket(&hash_secret(&query.ticket)).await?
        }
        "/v1/accept" => {
            let AuthDevice(device) = AuthDevice::from_request_parts(&mut parts, &state).await?;
            state.peer_for_device(&device.id).await?
        }
        _ => None,
    };
    let request = Request::from_parts(parts, body);
    if let Some(peer) = peer {
        return match tokio::time::timeout(Duration::from_secs(4), forward(request, peer)).await {
            Ok(Ok(response)) => Ok(response),
            _ => Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "relay peer unavailable",
            )),
        };
    }
    Ok(next.run(request).await)
}

async fn forward(mut request: Request, peer: SocketAddr) -> anyhow::Result<Response> {
    let client_upgrade = hyper::upgrade::on(&mut request);
    let mut outbound = Request::builder()
        .method(Method::GET)
        .uri(request.uri().path_and_query().unwrap().as_str())
        .header(header::HOST, peer.to_string())
        .header(header::CONNECTION, "upgrade")
        .body(Body::empty())?;
    for name in [
        header::AUTHORIZATION,
        header::UPGRADE,
        header::SEC_WEBSOCKET_KEY,
        header::SEC_WEBSOCKET_VERSION,
        header::SEC_WEBSOCKET_PROTOCOL,
    ] {
        for value in request.headers().get_all(&name) {
            outbound.headers_mut().append(name.clone(), value.clone());
        }
    }
    let tcp = TcpStream::connect(peer).await?;
    tcp.set_nodelay(true)?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let mut response = sender.send_request(outbound).await?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let peer_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            let upgrade = async { tokio::try_join!(client_upgrade, peer_upgrade) };
            if let Ok(Ok((client, upstream))) =
                tokio::time::timeout(Duration::from_secs(4), upgrade).await
            {
                tunnel(
                    TokioIo::new(client),
                    TokioIo::new(upstream),
                    Duration::from_secs(10),
                )
                .await;
            }
        });
    }
    Ok(response.map(Body::new))
}

/// A WebSocket tunnel has no useful half-closed state. Once either side
/// reaches EOF (after its final bytes are copied), drop both sockets.
async fn tunnel<A, B>(client: A, peer: B, write_timeout: Duration)
where
    A: AsyncRead + AsyncWrite,
    B: AsyncRead + AsyncWrite,
{
    let (mut client_rx, mut client_tx) = io::split(client);
    let (mut peer_rx, mut peer_tx) = io::split(peer);
    tokio::select! {
        _ = forward_bytes(&mut client_rx, &mut peer_tx, write_timeout) => {}
        _ = forward_bytes(&mut peer_rx, &mut client_tx, write_timeout) => {}
    }
}

async fn forward_bytes(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    write_timeout: Duration,
) -> io::Result<()> {
    let mut buf = [0; 8192];
    loop {
        let len = reader.read(&mut buf).await?;
        if len == 0 {
            return Ok(());
        }
        tokio::time::timeout(write_timeout, writer.write_all(&buf[..len])).await??;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn tunnel_releases_both_sockets_on_first_eof() {
        let (mut client, relay_client) = io::duplex(128);
        let (relay_peer, mut peer) = io::duplex(128);
        let task = tokio::spawn(tunnel(relay_client, relay_peer, Duration::from_secs(1)));
        client.write_all(b"final frame").await.unwrap();
        client.shutdown().await.unwrap();

        // The peer keeps its writer open, while still receiving the last
        // bytes and EOF from the relay.
        let mut data = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut data))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data, b"final frame");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn tunnel_releases_even_when_the_other_side_stops_reading() {
        let (mut client, relay_client) = io::duplex(128);
        let (relay_peer, _stalled_peer) = io::duplex(1);
        let task = tokio::spawn(tunnel(relay_client, relay_peer, Duration::from_millis(20)));
        client
            .write_all(b"larger than the peer buffer")
            .await
            .unwrap();
        let mut byte = [0];
        let count = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count, 0);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }
}

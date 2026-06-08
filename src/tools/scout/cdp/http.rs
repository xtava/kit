//! A tiny HTTP/1.1 GET over a raw socket — enough for CDP's `/json` discovery endpoints, with no
//! HTTP-client dependency. We read headers, then exactly `Content-Length` body bytes, so we don't
//! block waiting for a server that keeps the connection open (the DevTools endpoint does).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn get(port: u16, path: &str) -> Result<String> {
    timeout(HTTP_TIMEOUT, get_inner(port, path)).await.context("http get timed out")?
}

async fn get_inner(port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.context("connect")?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.context("write request")?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];

    let header_end = loop {
        if let Some(position) = find(&buf, b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).await.context("read headers")?;
        if read == 0 {
            bail!("connection closed before headers");
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let content_length = content_length(&headers);
    let mut body = buf[header_end..].to_vec();

    match content_length {
        Some(length) => {
            while body.len() < length {
                let read = stream.read(&mut chunk).await.context("read body")?;
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read]);
            }
            body.truncate(length);
        }
        None => {
            while stream.read(&mut chunk).await.context("read body")? != 0 {
                body.extend_from_slice(&chunk);
            }
        }
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

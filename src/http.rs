//! Minimal HTTP/1.1 for the console's local API (127.0.0.1 only): one request per connection, JSON bodies,
//! `Authorization: Bearer <token>`. Small on purpose (no extra dependency): no keep-alive, no chunked
//! bodies, bodies up to 1 MiB.

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const MAX_BODY: usize = 1 << 20;

pub struct Request {
    pub method: String,
    pub path: String,
    pub token: Option<String>,
    pub body: Vec<u8>,
}

pub async fn read_request<R: tokio::io::AsyncRead + Unpin>(r: R) -> Result<Request> {
    let mut br = BufReader::new(r);
    let mut line = String::new();
    br.read_line(&mut line).await?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else { bail!("bad request line") };
    let (method, path) = (method.to_string(), path.to_string());
    let mut len = 0usize;
    let mut token = None;
    loop {
        let mut h = String::new();
        if br.read_line(&mut h).await? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                len = v.parse().map_err(|_| anyhow!("bad content-length"))?;
            } else if k == "authorization" {
                token = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).map(|t| t.trim().to_string());
            }
        }
    }
    if len > MAX_BODY {
        bail!("body too large");
    }
    let mut body = vec![0u8; len];
    br.read_exact(&mut body).await?;
    Ok(Request { method, path, token, body })
}

pub async fn write_response<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, status: u16, body: &serde_json::Value) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let b = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    );
    w.write_all(head.as_bytes()).await?;
    w.write_all(b.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Blocking client side (the CLI): POST a JSON body, return the JSON answer.
pub fn post_blocking(stream: &mut std::net::TcpStream, path: &str, token: &str, body: &str) -> Result<serde_json::Value> {
    use std::io::{Read, Write};
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (_, payload) = text.split_once("\r\n\r\n").ok_or_else(|| anyhow!("malformed answer from the console"))?;
    Ok(serde_json::from_str(payload)?)
}

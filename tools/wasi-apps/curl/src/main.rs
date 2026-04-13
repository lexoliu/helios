use std::io::{self, Write};

use anyhow::{Context as _, Result, bail};
use helios_api::net::TcpStream;
use url::Url;

fn split_headers(response: &[u8]) -> Option<(&[u8], &[u8])> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    Some((&response[..split], &response[split + 4..]))
}

fn chunked_transfer(headers: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(headers).to_ascii_lowercase();
    lower.contains("transfer-encoding: chunked")
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();

    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("invalid chunked response: missing chunk-size line ending")?;
        let size_line = core::str::from_utf8(&body[..line_end])
            .context("invalid chunked response: chunk-size line was not utf-8")?;
        let size_hex = size_line
            .split(';')
            .next()
            .context("invalid chunked response: missing chunk-size")?
            .trim();
        let chunk_size = usize::from_str_radix(size_hex, 16)
            .with_context(|| format!("invalid chunk-size {size_hex:?}"))?;

        body = &body[line_end + 2..];
        if chunk_size == 0 {
            break;
        }
        if body.len() < chunk_size + 2 {
            bail!("invalid chunked response: truncated chunk payload");
        }

        decoded.extend_from_slice(&body[..chunk_size]);
        if &body[chunk_size..chunk_size + 2] != b"\r\n" {
            bail!("invalid chunked response: missing chunk payload terminator");
        }
        body = &body[chunk_size + 2..];
    }

    Ok(decoded)
}

async fn run() -> Result<()> {
    let raw = std::env::args().nth(1).context("usage: curl <http-url>")?;
    let url = Url::parse(&raw).with_context(|| format!("invalid URL: {raw}"))?;
    if url.scheme() != "http" {
        bail!("only http:// is supported");
    }
    let host = url.host_str().context("URL must include a host")?;
    let port = url.port_or_known_default().context("missing port")?;
    let mut target = url.path().to_owned();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }

    let stream = TcpStream::connect(host, port)
        .await
        .with_context(|| format!("tcp connect failed for {host}:{port}"))?;
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: helios-curl/0.1\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("failed to write request")?;

    let response = stream.read_to_end().await.context("failed to read response")?;
    if let Some((headers, body)) = split_headers(&response) {
        if chunked_transfer(headers) {
            let body = decode_chunked(body)?;
            io::stdout()
                .write_all(&body)
                .context("failed to write decoded response body")?;
        } else {
            io::stdout()
                .write_all(body)
                .context("failed to write response body")?;
        }
        return Ok(());
    }

    io::stdout()
        .write_all(&response)
        .context("failed to write response bytes")?;
    Ok(())
}

#[helios_api::main]
async fn main() -> Result<()> {
    if let Err(error) = run().await {
        let _ = writeln!(io::stderr(), "{error:#}");
    }
    Ok(())
}

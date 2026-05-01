use std::io::{self, Write};

use helios_api::net::TcpStream;
use thiserror::Error;
use url::Url;

type Result<T> = core::result::Result<T, CurlError>;

#[derive(Debug, Error)]
enum CurlError {
    #[error("usage: curl <http-url>")]
    Usage,
    #[error("invalid URL `{raw}`")]
    InvalidUrl {
        raw: String,
        #[source]
        source: url::ParseError,
    },
    #[error("only http:// is supported")]
    UnsupportedScheme,
    #[error("URL must include a host")]
    MissingHost,
    #[error("URL must include a port")]
    MissingPort,
    #[error("tcp connect failed for {host}:{port}")]
    TcpConnect {
        host: String,
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("failed to write request")]
    WriteRequest(#[source] io::Error),
    #[error("failed to read response")]
    ReadResponse(#[source] io::Error),
    #[error("failed to write response body")]
    WriteResponseBody(#[source] io::Error),
    #[error("invalid chunked response: missing chunk-size line ending")]
    MissingChunkSizeLineEnding,
    #[error("invalid chunked response: chunk-size line was not utf-8")]
    ChunkSizeUtf8(#[source] core::str::Utf8Error),
    #[error("invalid chunked response: missing chunk-size")]
    MissingChunkSize,
    #[error("invalid chunked response: invalid chunk-size")]
    InvalidChunkSize(#[source] core::num::ParseIntError),
    #[error("invalid chunked response: truncated chunk payload")]
    TruncatedChunkPayload,
    #[error("invalid chunked response: missing chunk payload terminator")]
    MissingChunkTerminator,
}

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
            .ok_or(CurlError::MissingChunkSizeLineEnding)?;
        let size_line = core::str::from_utf8(&body[..line_end])
            .map_err(CurlError::ChunkSizeUtf8)?;
        let size_hex = size_line
            .split(';')
            .next()
            .ok_or(CurlError::MissingChunkSize)?
            .trim();
        let chunk_size =
            usize::from_str_radix(size_hex, 16).map_err(CurlError::InvalidChunkSize)?;

        body = &body[line_end + 2..];
        if chunk_size == 0 {
            break;
        }
        if body.len() < chunk_size + 2 {
            return Err(CurlError::TruncatedChunkPayload);
        }

        decoded.extend_from_slice(&body[..chunk_size]);
        if &body[chunk_size..chunk_size + 2] != b"\r\n" {
            return Err(CurlError::MissingChunkTerminator);
        }
        body = &body[chunk_size + 2..];
    }

    Ok(decoded)
}

async fn run() -> Result<()> {
    let raw = std::env::args().nth(1).ok_or(CurlError::Usage)?;
    let url = Url::parse(&raw).map_err(|source| CurlError::InvalidUrl {
        raw: raw.clone(),
        source,
    })?;
    if url.scheme() != "http" {
        return Err(CurlError::UnsupportedScheme);
    }
    let host = url.host_str().ok_or(CurlError::MissingHost)?;
    let port = url.port_or_known_default().ok_or(CurlError::MissingPort)?;
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
        .map_err(|source| CurlError::TcpConnect {
            host: host.to_owned(),
            port,
            source,
        })?;
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: helios-curl/0.1\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(CurlError::WriteRequest)?;

    let response = stream.read_to_end().await.map_err(CurlError::ReadResponse)?;
    if let Some((headers, body)) = split_headers(&response) {
        if chunked_transfer(headers) {
            let body = decode_chunked(body)?;
            io::stdout()
                .write_all(&body)
                .map_err(CurlError::WriteResponseBody)?;
        } else {
            io::stdout()
                .write_all(body)
                .map_err(CurlError::WriteResponseBody)?;
        }
        return Ok(());
    }

    io::stdout()
        .write_all(&response)
        .map_err(CurlError::WriteResponseBody)?;
    Ok(())
}

#[helios_api::main]
async fn main() -> Result<()> {
    run().await
}

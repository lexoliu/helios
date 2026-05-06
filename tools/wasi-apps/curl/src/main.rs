use std::fs::File;
use std::io::{self, Write};

use helios_api::net::{DEFAULT_READ_CHUNK_BYTES, TcpStream};
use http::Uri;
use thiserror::Error;

type Result<T> = core::result::Result<T, CurlError>;

const NULL_DEVICE_PATH: &str = "/dev/null";

#[derive(Debug, Error)]
enum CurlError {
    #[error("usage: curl <http-url>")]
    Usage,
    #[error("curl option `{0}` requires a value")]
    MissingOptionValue(String),
    #[error("unsupported curl option `{0}`")]
    UnsupportedOption(String),
    #[error("multiple URLs were provided")]
    MultipleUrls,
    #[error("invalid URL `{raw}`")]
    InvalidUrl {
        raw: String,
        #[source]
        source: http::uri::InvalidUri,
    },
    #[error("only http:// is supported")]
    UnsupportedScheme,
    #[error("URL must include a host")]
    MissingHost,
    #[error("tcp connect failed for {host}:{port}: {source}")]
    TcpConnect {
        host: String,
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("failed to write request")]
    WriteRequest(#[source] io::Error),
    #[error("failed to read response after {bytes_read} bytes: {source}")]
    ReadResponse {
        bytes_read: usize,
        #[source]
        source: io::Error,
    },
    #[error("failed to write response body")]
    WriteResponseBody(#[source] io::Error),
    #[error("failed to create output file `{path}`")]
    CreateOutputFile {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("unsupported write-out variable in `{0}`")]
    UnsupportedWriteOut(String),
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

struct CurlOptions {
    url: String,
    output: OutputTarget,
    write_out: Option<String>,
}

enum OutputTarget {
    Stdout,
    Discard,
    File(File),
}

impl OutputTarget {
    fn from_path(path: String) -> Result<Self> {
        if path == NULL_DEVICE_PATH {
            return Ok(Self::Discard);
        }
        let file =
            File::create(&path).map_err(|source| CurlError::CreateOutputFile { path, source })?;
        Ok(Self::File(file))
    }

    fn write_body(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Stdout => {
                let mut stdout = io::stdout();
                stdout
                    .write_all(bytes)
                    .map_err(CurlError::WriteResponseBody)?;
                stdout.flush().map_err(CurlError::WriteResponseBody)
            }
            Self::Discard => Ok(()),
            Self::File(file) => file.write_all(bytes).map_err(CurlError::WriteResponseBody),
        }
    }
}

fn parse_options() -> Result<CurlOptions> {
    let mut args = std::env::args().skip(1);
    let mut url = None;
    let mut output = OutputTarget::Stdout;
    let mut write_out = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" | "-o" => {
                let path = args
                    .next()
                    .ok_or_else(|| CurlError::MissingOptionValue(arg.clone()))?;
                output = OutputTarget::from_path(path)?;
            }
            "--write-out" | "-w" => {
                write_out = Some(
                    args.next()
                        .ok_or_else(|| CurlError::MissingOptionValue(arg.clone()))?,
                );
            }
            _ if arg.starts_with('-') => return Err(CurlError::UnsupportedOption(arg)),
            _ => {
                if url.replace(arg).is_some() {
                    return Err(CurlError::MultipleUrls);
                }
            }
        }
    }

    Ok(CurlOptions {
        url: url.ok_or(CurlError::Usage)?,
        output,
        write_out,
    })
}

fn header_end(response: &[u8]) -> Option<usize> {
    response.windows(4).position(|window| window == b"\r\n\r\n")
}

fn chunked_transfer(headers: &[u8]) -> bool {
    headers
        .windows(b"transfer-encoding: chunked".len())
        .any(|window| {
            window
                .iter()
                .zip(b"transfer-encoding: chunked")
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();

    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or(CurlError::MissingChunkSizeLineEnding)?;
        let size_line =
            core::str::from_utf8(&body[..line_end]).map_err(CurlError::ChunkSizeUtf8)?;
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

async fn read_response_body(stream: &TcpStream, output: &mut OutputTarget) -> Result<usize> {
    let mut buffered = Vec::new();
    let header_split = loop {
        let Some(bytes) = stream
            .read(DEFAULT_READ_CHUNK_BYTES)
            .await
            .map_err(|source| CurlError::ReadResponse {
                bytes_read: buffered.len(),
                source,
            })?
        else {
            output.write_body(&buffered)?;
            return Ok(buffered.len());
        };
        buffered.extend_from_slice(&bytes);
        if let Some(header_split) = header_end(&buffered) {
            break header_split;
        }
    };

    let body_start = header_split + 4;
    let headers = buffered[..header_split].to_vec();
    let mut body = buffered.split_off(body_start);
    if chunked_transfer(&headers) {
        while let Some(bytes) = stream
            .read(DEFAULT_READ_CHUNK_BYTES)
            .await
            .map_err(|source| CurlError::ReadResponse {
                bytes_read: body.len(),
                source,
            })?
        {
            body.extend_from_slice(&bytes);
        }
        let decoded = decode_chunked(&body)?;
        let size = decoded.len();
        output.write_body(&decoded)?;
        return Ok(size);
    }

    let mut size = body.len();
    output.write_body(&body)?;
    while let Some(bytes) = stream
        .read(DEFAULT_READ_CHUNK_BYTES)
        .await
        .map_err(|source| CurlError::ReadResponse {
            bytes_read: size,
            source,
        })?
    {
        size += bytes.len();
        output.write_body(&bytes)?;
    }
    Ok(size)
}

fn write_out(template: &str, size_download: usize) -> Result<()> {
    let rendered = template.replace("%{size_download}", &size_download.to_string());
    if rendered.contains("%{") {
        return Err(CurlError::UnsupportedWriteOut(template.to_owned()));
    }
    let mut stdout = io::stdout();
    stdout
        .write_all(rendered.as_bytes())
        .map_err(CurlError::WriteResponseBody)?;
    stdout.flush().map_err(CurlError::WriteResponseBody)
}

async fn run() -> Result<()> {
    let mut options = parse_options()?;
    let uri: Uri = options
        .url
        .parse()
        .map_err(|source| CurlError::InvalidUrl {
            raw: options.url.clone(),
            source,
        })?;
    if uri.scheme_str() != Some("http") {
        return Err(CurlError::UnsupportedScheme);
    }
    let authority = uri.authority().ok_or(CurlError::MissingHost)?;
    let host = authority.host();
    let port = authority.port_u16().unwrap_or(80);
    let target = uri.path_and_query().map_or("/", |path| path.as_str());
    let host_header = authority.as_str();

    let stream = TcpStream::connect(host, port)
        .await
        .map_err(|source| CurlError::TcpConnect {
            host: host.to_owned(),
            port,
            source,
        })?;
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: helios-curl/0.1\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(CurlError::WriteRequest)?;

    let size_download = read_response_body(&stream, &mut options.output).await?;
    if let Some(template) = options.write_out.as_deref() {
        write_out(template, size_download)?;
    }
    Ok(())
}

#[helios_api::main]
async fn main() -> Result<()> {
    run().await
}

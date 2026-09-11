//! The Linux + Wasmtime side of the `curl-*` workloads: a curl-shaped
//! HTTP/1.1 client over `std::net::TcpStream`, which a plain WASI
//! runtime serves through `wasi:sockets`. The Helios `curl.wasm` itself
//! imports `helios:system/programs` and `wasi:http@0.3.0`, neither of
//! which upstream Wasmtime can instantiate, so the comparison uses this
//! client under `wasmtime run -S inherit-network` instead.
//!
//! The CLI contract mirrors `tools/wasi-apps/curl`: `--output`/`-o`
//! (`/dev/null` discards the body), `--write-out`/`-w` with
//! `%{size_download}`, and the body on stdout otherwise.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::num::ParseIntError;

use thiserror::Error;

type Result<T> = core::result::Result<T, CurlError>;

const NULL_DEVICE_PATH: &str = "/dev/null";
const USER_AGENT: &str = "helios-wasi-curl/0.1";

#[derive(Debug, Error)]
enum CurlError {
    #[error("usage: wasi-curl <http-url>")]
    Usage,
    #[error("curl option `{0}` requires a value")]
    MissingOptionValue(String),
    #[error("unsupported curl option `{0}`")]
    UnsupportedOption(String),
    #[error("multiple URLs were provided")]
    MultipleUrls,
    #[error("invalid URL `{raw}`: {reason}")]
    InvalidUrl { raw: String, reason: &'static str },
    #[error("invalid TCP port in `{raw}`")]
    InvalidPort {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("tcp connect failed for {address}: {source}")]
    TcpConnect {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("http request failed: {source}")]
    Request { source: io::Error },
    #[error("http response is malformed: {reason}")]
    MalformedResponse { reason: &'static str },
    #[error("http status {status} on `{url}`")]
    HttpStatus { status: u16, url: String },
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

struct HttpUrl {
    address: SocketAddr,
    authority: String,
    path: String,
}

fn parse_url(raw: &str) -> Result<HttpUrl> {
    let invalid = |reason: &'static str| CurlError::InvalidUrl {
        raw: raw.to_owned(),
        reason,
    };
    let rest = raw
        .strip_prefix("http://")
        .ok_or_else(|| invalid("only http:// URLs are supported"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| invalid("the authority needs a port"))?;
    let host: IpAddr = host
        .parse()
        .map_err(|_| invalid("the host must be an IP literal"))?;
    let port: u16 = port.parse().map_err(|source| CurlError::InvalidPort {
        raw: raw.to_owned(),
        source,
    })?;
    Ok(HttpUrl {
        address: SocketAddr::new(host, port),
        authority: authority.to_owned(),
        path,
    })
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

fn run() -> Result<()> {
    let mut options = parse_options()?;
    let url = parse_url(&options.url)?;
    let mut stream = TcpStream::connect(url.address).map_err(|source| CurlError::TcpConnect {
        address: url.address,
        source,
    })?;
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path, url.authority, USER_AGENT
    )
    .map_err(|source| CurlError::Request { source })?;

    // The host server answers with Content-Length and then closes, so the
    // response ends at EOF; headers and body are split at the first CRLF
    // pair boundary.
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|source| CurlError::Request { source })?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(CurlError::MalformedResponse {
            reason: "no header boundary",
        })?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .ok_or(CurlError::MalformedResponse {
            reason: "no status code",
        })?
        .parse()
        .map_err(|_| CurlError::MalformedResponse {
            reason: "unparsable status code",
        })?;
    if status != 200 {
        return Err(CurlError::HttpStatus {
            status,
            url: options.url.clone(),
        });
    }
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return Err(CurlError::MalformedResponse {
            reason: "chunked transfer encoding",
        });
    }

    let body = &response[split + 4..];
    let size_download = body.len();
    options.output.write_body(body)?;
    if let Some(template) = options.write_out.as_deref() {
        write_out(template, size_download)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    run()
}

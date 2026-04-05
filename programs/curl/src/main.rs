use std::env;
use std::io::{self, Cursor, Read, Write};
use std::time::Duration;

use helios_api::net::TcpStream;
use http::Uri;
use httparse::{Response, Status};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_PORT_HTTP: u16 = 80;

#[helios_api::main]
async fn main() -> io::Result<()> {
    let options = Options::parse(env::args())?;
    let endpoint = Endpoint::from_url(&options.url)?;
    let request = build_request(&endpoint);

    let stream = TcpStream::connect_timeout(&endpoint.host, endpoint.port, options.timeout).await?;
    stream.write_all(request.as_bytes()).await?;
    let response = stream.read_to_end().await?;
    stream.close().await?;

    let output = decode_response(&response, options.include_headers)?;
    std::io::stdout().lock().write_all(&output)?;
    Ok(())
}

struct Options {
    url: String,
    include_headers: bool,
    timeout: Duration,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> io::Result<Self> {
        let argv0 = args.next().unwrap_or_else(|| "curl".to_owned());
        let mut include_headers = false;
        let mut timeout = DEFAULT_TIMEOUT;
        let mut url = None;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-i" | "--include" => include_headers = true,
                "--timeout-ms" => {
                    let value = args.next().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("usage: {argv0} [-i|--include] [--timeout-ms <ms>] <http-url>"),
                        )
                    })?;
                    timeout = Duration::from_millis(value.parse().map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid timeout {value:?}: {error}"),
                        )
                    })?);
                }
                _ if arg.starts_with('-') => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown option {arg:?}"),
                    ));
                }
                _ if url.is_none() => url = Some(arg),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("usage: {argv0} [-i|--include] [--timeout-ms <ms>] <http-url>"),
                    ));
                }
            }
        }

        let Some(url) = url else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("usage: {argv0} [-i|--include] [--timeout-ms <ms>] <http-url>"),
            ));
        };

        Ok(Self {
            url,
            include_headers,
            timeout,
        })
    }
}

struct Endpoint {
    host: String,
    port: u16,
    authority: String,
    request_target: String,
}

impl Endpoint {
    fn from_url(input: &str) -> io::Result<Self> {
        let uri: Uri = input.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid url {input:?}: {error}"),
            )
        })?;
        if uri.scheme_str() != Some("http") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unsupported url scheme {:?}; only http is supported",
                    uri.scheme_str()
                ),
            ));
        }
        let host = uri.host().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("url {input:?} is missing a host"),
            )
        })?;
        let port = uri.port_u16().unwrap_or(DEFAULT_PORT_HTTP);
        let authority = if port == DEFAULT_PORT_HTTP {
            host.to_owned()
        } else {
            format!("{host}:{port}")
        };

        let request_target = uri.path_and_query().map_or("/", |value| value.as_str());

        Ok(Self {
            host: host.to_owned(),
            port,
            authority,
            request_target: request_target.to_owned(),
        })
    }
}

fn build_request(endpoint: &Endpoint) -> String {
    format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: helios-curl/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        endpoint.request_target, endpoint.authority
    )
}

fn decode_response(response: &[u8], include_headers: bool) -> io::Result<Vec<u8>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = Response::new(&mut headers);
    let header_len = match parsed.parse(response).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse http response: {error}"),
        )
    })? {
        Status::Complete(length) => length,
        Status::Partial => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "http response ended before headers completed",
            ));
        }
    };

    let mut body = if parsed.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("transfer-encoding")
            && header.value.eq_ignore_ascii_case(b"chunked")
    }) {
        let mut decoded = Vec::new();
        chunked_transfer::Decoder::new(Cursor::new(&response[header_len..]))
            .read_to_end(&mut decoded)?;
        decoded
    } else {
        response[header_len..].to_vec()
    };

    if include_headers {
        let mut output = response[..header_len].to_vec();
        output.append(&mut body);
        return Ok(output);
    }

    Ok(body)
}

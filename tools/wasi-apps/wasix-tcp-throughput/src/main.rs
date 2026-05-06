use std::num::ParseIntError;

use thiserror::Error;

type Result<T> = core::result::Result<T, WasixTcpThroughputError>;

const BUFFER_BYTES: usize = 64 * 1024;
const WASIX_ADDRESS_FAMILY_IP_INET4: u8 = 1;
const WASIX_SOCK_TYPE_STREAM: i32 = 1;
const WASIX_IPPROTO_TCP: i32 = 6;
const ERRNO_SUCCESS: i32 = 0;

#[repr(C)]
struct WasixAddrPort {
    family: u8,
    padding: u8,
    port: u16,
    ipv4: [u8; 4],
    trailing: [u8; 14],
}

#[repr(C)]
struct Iovec {
    buf: *mut u8,
    buf_len: usize,
}

#[derive(Debug, Error)]
enum WasixTcpThroughputError {
    #[error("usage: wasix-tcp-throughput <ipv4-host> <port> <expected-bytes>")]
    Usage,
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
    #[error("invalid IPv4 address `{0}`")]
    InvalidIpv4(String),
    #[error("invalid TCP port `{raw}`")]
    InvalidPort {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("invalid expected byte count `{raw}`")]
    InvalidExpectedBytes {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("{syscall} failed with errno {errno}")]
    Syscall { syscall: &'static str, errno: i32 },
    #[error("tcp stream delivered {actual} bytes, expected {expected}")]
    UnexpectedByteCount { actual: usize, expected: usize },
}

#[link(wasm_import_module = "wasix_32v1")]
unsafe extern "C" {
    #[link_name = "sock_open"]
    fn wasix_sock_open(af: i32, socktype: i32, proto: i32, ret_fd: *mut i32) -> i32;
    #[link_name = "sock_connect"]
    fn wasix_sock_connect(fd: i32, addr: *const WasixAddrPort) -> i32;
}

#[link(wasm_import_module = "wasi_snapshot_preview1")]
unsafe extern "C" {
    #[link_name = "sock_recv"]
    fn wasix_sock_recv(
        fd: i32,
        ri_data: *const Iovec,
        ri_data_len: usize,
        ri_flags: u16,
        ro_datalen: *mut usize,
        ro_flags: *mut u16,
    ) -> i32;
    #[link_name = "fd_close"]
    fn wasix_fd_close(fd: i32) -> i32;
}

struct Args {
    host: [u8; 4],
    port: u16,
    expected_bytes: usize,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let host_raw = args.next().ok_or(WasixTcpThroughputError::Usage)?;
    let port_raw = args.next().ok_or(WasixTcpThroughputError::Usage)?;
    let expected_raw = args.next().ok_or(WasixTcpThroughputError::Usage)?;
    if let Some(extra) = args.next() {
        return Err(WasixTcpThroughputError::UnexpectedArgument(extra));
    }
    let host = parse_ipv4(&host_raw)?;
    let port = port_raw
        .parse()
        .map_err(|source| WasixTcpThroughputError::InvalidPort {
            raw: port_raw,
            source,
        })?;
    let expected_bytes =
        expected_raw
            .parse()
            .map_err(|source| WasixTcpThroughputError::InvalidExpectedBytes {
                raw: expected_raw,
                source,
            })?;
    Ok(Args {
        host,
        port,
        expected_bytes,
    })
}

fn parse_ipv4(raw: &str) -> Result<[u8; 4]> {
    let mut octets = [0_u8; 4];
    let mut count = 0;
    for (index, part) in raw.split('.').enumerate() {
        if index >= octets.len() {
            return Err(WasixTcpThroughputError::InvalidIpv4(raw.to_owned()));
        }
        octets[index] = part
            .parse()
            .map_err(|_| WasixTcpThroughputError::InvalidIpv4(raw.to_owned()))?;
        count += 1;
    }
    if count != octets.len() {
        return Err(WasixTcpThroughputError::InvalidIpv4(raw.to_owned()));
    }
    Ok(octets)
}

fn ensure_success(syscall: &'static str, errno: i32) -> Result<()> {
    if errno == ERRNO_SUCCESS {
        Ok(())
    } else {
        Err(WasixTcpThroughputError::Syscall { syscall, errno })
    }
}

fn open_tcp_socket() -> Result<i32> {
    let mut fd = 0_i32;
    let errno = unsafe {
        wasix_sock_open(
            i32::from(WASIX_ADDRESS_FAMILY_IP_INET4),
            WASIX_SOCK_TYPE_STREAM,
            WASIX_IPPROTO_TCP,
            &mut fd,
        )
    };
    ensure_success("sock_open", errno)?;
    Ok(fd)
}

fn connect(fd: i32, host: [u8; 4], port: u16) -> Result<()> {
    let addr = WasixAddrPort {
        family: WASIX_ADDRESS_FAMILY_IP_INET4,
        padding: 0,
        port,
        ipv4: host,
        trailing: [0; 14],
    };
    let errno = unsafe { wasix_sock_connect(fd, &addr) };
    ensure_success("sock_connect", errno)
}

fn receive(fd: i32, buffer: &mut [u8]) -> Result<usize> {
    let iov = Iovec {
        buf: buffer.as_mut_ptr(),
        buf_len: buffer.len(),
    };
    let mut datalen = 0_usize;
    let mut flags = 0_u16;
    let errno = unsafe { wasix_sock_recv(fd, &iov, 1, 0, &mut datalen, &mut flags) };
    ensure_success("sock_recv", errno)?;
    Ok(datalen)
}

fn close(fd: i32) {
    let _ = unsafe { wasix_fd_close(fd) };
}

fn run() -> Result<()> {
    let args = parse_args()?;
    let fd = open_tcp_socket()?;
    let outcome = (|| {
        connect(fd, args.host, args.port)?;
        let mut buffer = vec![0_u8; BUFFER_BYTES];
        let mut total = 0_usize;
        loop {
            let read = receive(fd, &mut buffer)?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read)
                .expect("tcp byte counter overflowed");
        }
        if total != args.expected_bytes {
            return Err(WasixTcpThroughputError::UnexpectedByteCount {
                actual: total,
                expected: args.expected_bytes,
            });
        }
        println!("wasix-tcp-throughput:{total}");
        Ok(())
    })();
    close(fd);
    outcome
}

fn main() -> Result<()> {
    run()
}

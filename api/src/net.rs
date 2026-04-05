//! Network syscalls.

use std::io;
use std::time::Duration;

use crate::bindings::helios::system::net as raw;

pub use crate::bindings::helios::system::net::{IpAddress, Ipv4Address, PingErrorKind, PingReply};

pub async fn ping(host: &str, timeout: Duration) -> io::Result<PingReply> {
    raw::ping(host.to_owned(), duration_to_nanos(timeout))
        .await
        .map_err(|error| io::Error::other(format!("{:?}: {}", error.kind, error.detail)))
}

fn duration_to_nanos(duration: Duration) -> u64 {
    duration
        .as_nanos()
        .try_into()
        .expect("duration does not fit into wasi nanoseconds")
}

//! The guest debugger's accept loop: one listener, one session at a time.
//!
//! A session and the listener fail differently. A transport or protocol
//! fault in a session (a reset, an end of stream in the middle of a
//! frame, an `Open` on an invocation that is already open) belongs to the
//! peer that caused it, so it ends that session and nothing else. Only the
//! listener failing to accept leaves the debugger with no way to be
//! reached again, so that alone ends [`serve_sessions`].

use std::convert::Infallible;
use std::future::Future;
use std::io;

use futures_io::{AsyncRead, AsyncWrite};
use thiserror::Error;

use crate::error::DispatchError;
use crate::system::server::{Dispatcher, serve};

/// A source of inspector connections.
pub trait Listener {
    type Connection: Connection;

    /// The fault that makes the listener unable to accept any further
    /// connection.
    type Error;

    /// Waits for the next connection.
    fn accept(&mut self) -> impl Future<Output = Result<Self::Connection, Self::Error>>;
}

/// One accepted transport.
pub trait Connection {
    type Read: AsyncRead + Unpin;
    type Write: AsyncWrite + Unpin;

    /// The halves the RPC framing runs over. Called once per connection.
    fn halves(&mut self) -> (Self::Read, Self::Write);

    /// Releases the transport once its session has ended.
    fn close(self) -> impl Future<Output = io::Result<()>>;
}

/// A session that ended because of a fault rather than because the peer
/// closed the transport.
#[derive(Debug, Error)]
#[error("inspector session failed: {0}")]
pub struct SessionError(#[source] DispatchError);

/// What [`serve_sessions`] reports about a session that did not end
/// cleanly.
#[derive(Debug)]
pub enum SessionFault {
    /// The session's RPC loop failed.
    Failed(SessionError),
    /// The session ended but its transport could not be released.
    CloseFailed(io::Error),
}

/// Accepts connections from `listener` and serves each to its end, one at
/// a time, with a clone of `dispatcher`.
///
/// A fault in a session is handed to `report` and the next connection is
/// accepted. The loop never returns `Ok`; it returns the listener's own
/// error when accepting fails.
pub async fn serve_sessions<L, D>(
    mut listener: L,
    dispatcher: D,
    mut report: impl FnMut(SessionFault),
) -> Result<Infallible, L::Error>
where
    L: Listener,
    D: Dispatcher + Clone,
{
    loop {
        let mut connection = listener.accept().await?;
        let (read, write) = connection.halves();
        // One session at a time: the protocol multiplexes its own
        // invocations, so a second transport would be a second view of
        // the same machine rather than more concurrency.
        if let Err(error) = serve(read, write, dispatcher.clone()).await {
            report(SessionFault::Failed(SessionError(error)));
        }
        if let Err(error) = connection.close().await {
            report(SessionFault::CloseFailed(error));
        }
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};
    use tokio::sync::mpsc;
    use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use super::{Connection, Listener, SessionFault, serve_sessions};
    use crate::error::DispatchError;
    use crate::system::server::{Dispatcher, Invocations};
    use crate::transport::Client;
    use crate::wire::{Frame, write_frame};

    const INSTANCE: &str = "server:test/probe";
    const FUNC: &str = "ping";

    /// Long enough for a loaded machine, short enough that a loop which
    /// stopped accepting fails the test instead of hanging the suite.
    const DEADLINE: Duration = Duration::from_secs(5);

    #[derive(Clone)]
    struct Probe;

    impl Dispatcher for Probe {
        fn supports(&self, instance: &str, func: &str) -> bool {
            (instance, func) == (INSTANCE, FUNC)
        }

        async fn dispatch(
            &self,
            _invocations: &Invocations,
            _instance: &str,
            _func: &str,
            _payload: &[u8],
        ) -> Result<Vec<u8>, DispatchError> {
            Ok(b"pong".to_vec())
        }
    }

    struct MemoryConnection(Option<DuplexStream>);

    impl Connection for MemoryConnection {
        type Read = Compat<ReadHalf<DuplexStream>>;
        type Write = Compat<WriteHalf<DuplexStream>>;

        fn halves(&mut self) -> (Self::Read, Self::Write) {
            let stream = self.0.take().expect("a connection is split once");
            let (read, write) = tokio::io::split(stream);
            (read.compat(), write.compat_write())
        }

        async fn close(self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Hands out the connections the test pushes, and fails once the test
    /// has dropped its sender, which is how the test ends the loop.
    struct MemoryListener(mpsc::UnboundedReceiver<DuplexStream>);

    #[derive(Debug, PartialEq, Eq)]
    struct ListenerClosed;

    impl Listener for MemoryListener {
        type Connection = MemoryConnection;
        type Error = ListenerClosed;

        async fn accept(&mut self) -> Result<MemoryConnection, ListenerClosed> {
            self.0
                .recv()
                .await
                .map(|stream| MemoryConnection(Some(stream)))
                .ok_or(ListenerClosed)
        }
    }

    /// A session that resets mid-frame and one that opens an invocation
    /// twice each end alone; the next connection is served.
    #[test]
    fn a_failed_session_does_not_end_the_listener() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (connections, incoming) = mpsc::unbounded_channel();
                let mut faults = Vec::new();
                let server = serve_sessions(MemoryListener(incoming), Probe, |fault| {
                    faults.push(fault);
                });

                let peers = async {
                    // Reset mid-frame: half of a valid frame, then the
                    // transport goes away.
                    let (mut peer, guest) = tokio::io::duplex(4096);
                    connections.send(guest).expect("the listener is alive");
                    let mut bytes = Vec::new();
                    write_frame(
                        &mut bytes,
                        &Frame::Open {
                            invocation: 1,
                            instance: INSTANCE.to_owned(),
                            func: FUNC.to_owned(),
                        },
                    )
                    .await
                    .expect("encoding into memory succeeds");
                    peer.write_all(&bytes[..bytes.len() / 2])
                        .await
                        .expect("the guest side is reading");
                    drop(peer);

                    // The same invocation opened twice.
                    let (mut peer, guest) = tokio::io::duplex(4096);
                    connections.send(guest).expect("the listener is alive");
                    {
                        let mut write = (&mut peer).compat_write();
                        for _ in 0..2 {
                            write_frame(
                                &mut write,
                                &Frame::Open {
                                    invocation: 7,
                                    instance: INSTANCE.to_owned(),
                                    func: FUNC.to_owned(),
                                },
                            )
                            .await
                            .expect("the guest side is reading");
                        }
                    }
                    // Keep the transport open until the guest has judged
                    // the second `Open`; the guest ends the session.
                    let mut sink = Vec::new();
                    let _ = tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut sink).await;

                    // A fresh session completes a request.
                    let (host, guest) = tokio::io::duplex(4096);
                    connections.send(guest).expect("the listener is alive");
                    let (read, write) = tokio::io::split(host);
                    let client = Client::new(read.compat(), write.compat_write());
                    let answer = tokio::time::timeout(
                        DEADLINE,
                        client.invoke_raw(INSTANCE, FUNC, Vec::new()),
                    )
                    .await
                    .expect("the debugger accepted the next connection")
                    .expect("the request completes");
                    assert_eq!(answer, b"pong");
                    drop(client);
                    drop(connections);
                };

                let (outcome, ()) =
                    tokio::time::timeout(DEADLINE, futures_util::future::join(server, peers))
                        .await
                        .expect("the accept loop ended");
                assert_eq!(outcome.unwrap_err(), ListenerClosed);
                // Both broken sessions were reported with their cause,
                // in order, and neither was a close failure.
                assert_eq!(faults.len(), 2, "faults: {faults:?}");
                assert!(
                    matches!(&faults[0], SessionFault::Failed(error)
                        if error.to_string().contains("read debugger request frame")),
                    "mid-frame reset reported as {:?}",
                    faults[0]
                );
                assert!(
                    matches!(&faults[1], SessionFault::Failed(error)
                        if error.to_string().contains("already open")),
                    "duplicate open reported as {:?}",
                    faults[1]
                );
            });
    }
}

//! The guest side of the inspector RPC: one connection, many invocations.
//!
//! The wire carries an invocation number on every frame and the host
//! demultiplexes on it, so the two ends may have any number of calls in
//! flight at once. This loop is the guest half of that: it reads frames
//! continuously, starts each completed request as a future of its own,
//! and writes whichever response finishes first.
//!
//! The concurrency lives inside a single future on purpose. A guest
//! component is driven by the component model's export executor, which
//! polls the exported call's future and then sleeps on the instance's
//! waitable set whenever that poll returns `Pending` without the task's
//! waker having been signalled. Work handed to `wit_bindgen::spawn` is
//! appended to the executor's task list *after* the poll that queued it
//! has already returned `Pending`, and nothing signals the waker, so the
//! executor sleeps with the new task never polled — it only runs once an
//! unrelated waitable event happens to wake the instance. Keeping every
//! in-flight invocation inside one future keeps every wakeup an ordinary
//! intra-future wakeup that the executor already handles.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::future::Future;
use std::io;

use futures_io::{AsyncRead, AsyncWrite};
use futures_util::future::{Either, select};
use futures_util::stream::{FuturesUnordered, StreamExt as _};

use crate::error::DispatchError;
use crate::wire::{Frame, read_frame, write_frame};

/// Bytes of a response body carried by one `Data` frame.
const RESPONSE_CHUNK_BYTES: usize = 64 * 1024;

/// The methods a [`serve`] loop answers.
///
/// The returned future is deliberately not `Send`: in the guest it is
/// polled by the component model's single-threaded export executor and
/// the host calls it awaits are not `Send` either.
pub trait Dispatcher {
    /// Whether this dispatcher answers `instance.func` at all.
    fn supports(&self, instance: &str, func: &str) -> bool;

    /// Runs one invocation whose request payload has been fully read.
    fn dispatch(
        &self,
        instance: &str,
        func: &str,
        payload: &[u8],
    ) -> impl Future<Output = Result<Vec<u8>, DispatchError>>;
}

/// A request whose `Open` frame has been accepted and whose payload is
/// still arriving.
struct PendingRequest {
    instance: String,
    func: String,
    payload: Vec<u8>,
}

/// An invocation that has run to completion and owes the host a reply.
struct Completed {
    invocation: u32,
    result: Result<Vec<u8>, DispatchError>,
}

/// Whichever half of the loop produced work this turn.
enum Ready<R> {
    /// The reader resolved, handing back the transport it owned.
    Frame(R, io::Result<Option<Frame>>),
    Completed(Completed),
}

/// What reading one frame asks the loop to do next.
enum FrameAction {
    Continue,
    Dispatch(u32, PendingRequest),
}

/// Serves inspector RPC over `read`/`write` until the transport closes.
///
/// Invocations run concurrently, so a long call such as `programs.exec`
/// does not stop the connection from answering `stats.snapshot` or
/// `tracing.recent` while it runs. Frames are written one at a time by
/// this single loop, so a response never interleaves with another.
pub async fn serve<R, W, D>(read: R, mut write: W, dispatcher: D) -> Result<(), DispatchError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    D: Dispatcher,
{
    let mut requests = BTreeMap::<u32, PendingRequest>::new();
    let mut inflight = FuturesUnordered::new();
    let mut next_frame = Box::pin(read_next_frame(read));
    // Cleared once the transport reports end of stream.
    let mut reading = true;

    loop {
        let ready = if !reading {
            // The transport is gone, so there is nobody to answer, but
            // a program an invocation started is still running: the
            // remaining invocations are driven to completion and their
            // responses dropped rather than cancelled mid-run.
            match inflight.next().await {
                Some(_) => continue,
                None => return Ok(()),
            }
        } else if inflight.is_empty() {
            let (reader, frame) = next_frame.as_mut().await;
            Ready::Frame(reader, frame)
        } else {
            match select(next_frame.as_mut(), inflight.next()).await {
                Either::Left(((reader, frame), _)) => Ready::Frame(reader, frame),
                Either::Right((Some(completed), _)) => Ready::Completed(completed),
                Either::Right((None, _)) => {
                    unreachable!("a non-empty invocation set never ends")
                }
            }
        };

        match ready {
            Ready::Frame(reader, frame) => {
                next_frame.set(read_next_frame(reader));
                let frame = frame.map_err(|source| DispatchError::Io {
                    operation: "read debugger request frame",
                    source,
                })?;
                match frame {
                    None => reading = false,
                    Some(frame) => {
                        let action =
                            handle_frame(&mut write, &dispatcher, &mut requests, frame).await?;
                        if let FrameAction::Dispatch(invocation, request) = action {
                            inflight.push(run_invocation(&dispatcher, invocation, request));
                        }
                    }
                }
            }
            Ready::Completed(completed) => {
                write_completion(&mut write, completed).await?;
            }
        }
    }
}

/// Reads one frame and hands the transport back, so the read future can
/// be raced against the in-flight invocations without ever being
/// cancelled part way through a frame.
async fn read_next_frame<R>(mut read: R) -> (R, io::Result<Option<Frame>>)
where
    R: AsyncRead + Unpin,
{
    let frame = read_frame(&mut read).await;
    (read, frame)
}

/// Runs one invocation and labels its result with the invocation it
/// belongs to, which is what lets responses complete out of order.
async fn run_invocation<D>(dispatcher: &D, invocation: u32, request: PendingRequest) -> Completed
where
    D: Dispatcher,
{
    let result = dispatcher
        .dispatch(&request.instance, &request.func, &request.payload)
        .await;
    Completed { invocation, result }
}

async fn handle_frame<W, D>(
    write: &mut W,
    dispatcher: &D,
    requests: &mut BTreeMap<u32, PendingRequest>,
    frame: Frame,
) -> Result<FrameAction, DispatchError>
where
    W: AsyncWrite + Unpin,
    D: Dispatcher,
{
    match frame {
        Frame::Open {
            invocation,
            instance,
            func,
        } => {
            if !dispatcher.supports(&instance, &func) {
                write_frame(
                    write,
                    &Frame::Reject {
                        invocation,
                        message: format!(
                            "remote invocation {instance}.{func} is not exposed by the embedded debugger"
                        ),
                    },
                )
                .await
                .map_err(|source| DispatchError::Io {
                    operation: "reject unsupported debugger request",
                    source,
                })?;
                return Ok(FrameAction::Continue);
            }
            match requests.entry(invocation) {
                Entry::Occupied(_) => {
                    return Err(DispatchError::protocol(format!(
                        "invocation {invocation} was opened while it was already open"
                    )));
                }
                Entry::Vacant(slot) => {
                    slot.insert(PendingRequest {
                        instance,
                        func,
                        payload: Vec::new(),
                    });
                }
            }
            write_frame(write, &Frame::Accept { invocation })
                .await
                .map_err(|source| DispatchError::Io {
                    operation: "accept debugger request stream",
                    source,
                })?;
            Ok(FrameAction::Continue)
        }
        Frame::Data {
            invocation,
            path,
            payload,
        } => {
            reject_nested_path(&path)?;
            let request = requests.get_mut(&invocation).ok_or_else(|| {
                DispatchError::protocol(format!(
                    "received payload for invocation {invocation}, which is not open"
                ))
            })?;
            request.payload.extend_from_slice(&payload);
            Ok(FrameAction::Continue)
        }
        Frame::Close { invocation, path } => {
            reject_nested_path(&path)?;
            let request = requests.remove(&invocation).ok_or_else(|| {
                DispatchError::protocol(format!(
                    "received close for invocation {invocation}, which is not open"
                ))
            })?;
            Ok(FrameAction::Dispatch(invocation, request))
        }
        Frame::Accept { .. } | Frame::Reject { .. } => Err(DispatchError::protocol(
            "unexpected control frame on the debugger request stream",
        )),
    }
}

fn reject_nested_path(path: &[u32]) -> Result<(), DispatchError> {
    if path.is_empty() {
        return Ok(());
    }
    Err(DispatchError::protocol(
        "nested request stream paths are unsupported in the guest debugger",
    ))
}

async fn write_completion<W>(write: &mut W, completed: Completed) -> Result<(), DispatchError>
where
    W: AsyncWrite + Unpin,
{
    let Completed { invocation, result } = completed;
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            return write_frame(
                write,
                &Frame::Reject {
                    invocation,
                    message: format!("{error}"),
                },
            )
            .await
            .map_err(|source| DispatchError::Io {
                operation: "report debugger request failure",
                source,
            });
        }
    };
    for chunk in response.chunks(RESPONSE_CHUNK_BYTES) {
        write_frame(
            write,
            &Frame::Data {
                invocation,
                path: Vec::new(),
                payload: chunk.to_vec(),
            },
        )
        .await
        .map_err(|source| DispatchError::Io {
            operation: "write debugger response payload",
            source,
        })?;
    }
    write_frame(
        write,
        &Frame::Close {
            invocation,
            path: Vec::new(),
        },
    )
    .await
    .map_err(|source| DispatchError::Io {
        operation: "close debugger response stream",
        source,
    })
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::future::{Either, select};
    use tokio::sync::Notify;
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use super::{Dispatcher, serve};
    use crate::error::DispatchError;
    use crate::transport::Client;

    const SLOW_INSTANCE: &str = "server:test/slow";
    const SLOW_FUNC: &str = "run";
    const PROBE_INSTANCE: &str = "server:test/probe";
    const PROBE_FUNC: &str = "ping";

    /// How long a probe may take while the slow invocation is still
    /// running in the guest. Long enough that a loaded machine does not
    /// fail it, short enough that a server which answers one invocation
    /// at a time fails the test instead of hanging the suite.
    const PROBE_DEADLINE: Duration = Duration::from_secs(5);

    /// A dispatcher with one call that does not finish until it is told
    /// to and one that answers immediately. That is the shape of the
    /// defect: `programs.exec` runs for as long as the program it
    /// started, while `stats.snapshot` has its answer ready.
    struct BlockingDispatcher {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl Dispatcher for BlockingDispatcher {
        fn supports(&self, instance: &str, func: &str) -> bool {
            matches!(
                (instance, func),
                (SLOW_INSTANCE, SLOW_FUNC) | (PROBE_INSTANCE, PROBE_FUNC)
            )
        }

        async fn dispatch(
            &self,
            instance: &str,
            func: &str,
            payload: &[u8],
        ) -> Result<Vec<u8>, DispatchError> {
            match (instance, func) {
                (SLOW_INSTANCE, SLOW_FUNC) => {
                    assert_eq!(payload, b"go");
                    self.started.notify_one();
                    self.release.notified().await;
                    Ok(b"slow".to_vec())
                }
                (PROBE_INSTANCE, PROBE_FUNC) => Ok(b"pong".to_vec()),
                _ => Err(DispatchError::protocol("unsupported test method")),
            }
        }
    }

    /// An invocation still running in the guest must not stop the
    /// connection from answering another one.
    ///
    /// This is the shape the inspector actually produces: a workload
    /// that misses its deadline has its call abandoned host-side while
    /// the guest goes on running it, and the diagnostics the lane needs
    /// — `stats.snapshot`, `tracing.recent` — are asked for on the same
    /// connection right afterwards. A server that reads the next frame
    /// only after the previous dispatch returned never reads them.
    #[test]
    fn a_running_invocation_does_not_block_the_connection() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (guest, host) = tokio::io::duplex(4096);
                let (guest_read, guest_write) = tokio::io::split(guest);
                let (host_read, host_write) = tokio::io::split(host);
                let client = Client::new(host_read.compat(), host_write.compat_write());

                let started = Arc::new(Notify::new());
                let release = Arc::new(Notify::new());
                let server = serve(
                    guest_read.compat(),
                    guest_write.compat_write(),
                    BlockingDispatcher {
                        started: Arc::clone(&started),
                        release: Arc::clone(&release),
                    },
                );

                let session = async {
                    let slow =
                        Box::pin(client.invoke_raw(SLOW_INSTANCE, SLOW_FUNC, b"go".to_vec()));
                    match select(slow, Box::pin(started.notified())).await {
                        Either::Left((outcome, _)) => {
                            panic!("the slow invocation answered before it began: {outcome:?}")
                        }
                        // Abandoning the call is what the inspector's
                        // workload deadline does: the host stops
                        // waiting, the guest keeps running.
                        Either::Right(((), abandoned)) => drop(abandoned),
                    }

                    let answer = probe(&client).await;
                    assert_eq!(answer, b"pong");

                    // The abandoned invocation still owes a response.
                    // Letting it finish proves the server writes it
                    // without disturbing the connection it shares.
                    release.notify_one();
                    let answer = probe(&client).await;
                    assert_eq!(answer, b"pong");
                };

                match select(Box::pin(server), Box::pin(session)).await {
                    Either::Left((outcome, _)) => {
                        panic!("the server loop ended before the session completed: {outcome:?}")
                    }
                    Either::Right(((), _)) => (),
                }
            });
    }

    async fn probe<R, W>(client: &Client<R, W>) -> Vec<u8>
    where
        R: futures_io::AsyncRead + Send + Unpin + 'static,
        W: futures_io::AsyncWrite + Send + Unpin + 'static,
    {
        tokio::time::timeout(
            PROBE_DEADLINE,
            client.invoke_raw(PROBE_INSTANCE, PROBE_FUNC, Vec::new()),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the probe went unanswered for {PROBE_DEADLINE:?} while an invocation was still \
                 running in the guest"
            )
        })
        .unwrap_or_else(|error| panic!("probe invocation failed: {error}"))
    }
}

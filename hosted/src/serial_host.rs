use std::future::Future;
use std::io::{self, Read, Write};
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
#[cfg(test)]
use tokio::io::{DuplexStream, ReadHalf, WriteHalf, split};
use wasmtime::component::{Accessor, HasSelf, Resource};

use crate::init_program::StoreData;

type IoFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

trait HostedSerialBackend: Send + Sync {
    fn read(&self, max_bytes: u32) -> IoFuture<'_, Vec<u8>>;
    fn write(&self, bytes: Vec<u8>) -> IoFuture<'_, ()>;
    fn flush(&self) -> IoFuture<'_, ()>;
}

#[derive(Clone)]
pub struct HostedSerialIo {
    backend: Arc<dyn HostedSerialBackend>,
}

pub struct HostedSerialPort {
    io: HostedSerialIo,
}

struct StdioSerialBackend {
    input: Arc<Mutex<std::io::Stdin>>,
    output: Arc<Mutex<std::io::Stdout>>,
}

#[cfg(test)]
struct DuplexSerialBackend {
    read: tokio::sync::Mutex<ReadHalf<DuplexStream>>,
    write: tokio::sync::Mutex<WriteHalf<DuplexStream>>,
    #[cfg(test)]
    trace: Arc<SerialTrace>,
}

#[cfg(test)]
pub(crate) struct SerialTrace {
    reads: AtomicUsize,
    writes: AtomicUsize,
    flushes: AtomicUsize,
    read_bytes: Mutex<Vec<Vec<u8>>>,
    write_bytes: Mutex<Vec<Vec<u8>>>,
}

impl HostedSerialIo {
    pub fn new() -> Self {
        Self {
            backend: Arc::new(StdioSerialBackend {
                input: Arc::new(Mutex::new(std::io::stdin())),
                output: Arc::new(Mutex::new(std::io::stdout())),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn duplex_pair(buffer_size: usize) -> (Self, DuplexStream) {
        let (io, peer, _) = Self::traced_duplex_pair(buffer_size);
        (io, peer)
    }

    #[cfg(test)]
    pub(crate) fn traced_duplex_pair(buffer_size: usize) -> (Self, DuplexStream, Arc<SerialTrace>) {
        let (host, peer) = tokio::io::duplex(buffer_size);
        let (read, write) = split(host);
        let trace = Arc::new(SerialTrace::new());
        (
            Self {
                backend: Arc::new(DuplexSerialBackend {
                    read: tokio::sync::Mutex::new(read),
                    write: tokio::sync::Mutex::new(write),
                    trace: trace.clone(),
                }),
            },
            peer,
            trace,
        )
    }
}

#[cfg(test)]
impl SerialTrace {
    fn new() -> Self {
        Self {
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
            read_bytes: Mutex::new(Vec::new()),
            write_bytes: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    pub(crate) fn writes(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }

    pub(crate) fn flushes(&self) -> usize {
        self.flushes.load(Ordering::Relaxed)
    }

    pub(crate) fn read_bytes(&self) -> Vec<Vec<u8>> {
        self.read_bytes
            .lock()
            .unwrap_or_else(|_| panic!("serial trace read-bytes mutex poisoned"))
            .clone()
    }

    pub(crate) fn write_bytes(&self) -> Vec<Vec<u8>> {
        self.write_bytes
            .lock()
            .unwrap_or_else(|_| panic!("serial trace write-bytes mutex poisoned"))
            .clone()
    }
}

impl HostedSerialBackend for StdioSerialBackend {
    fn read(&self, max_bytes: u32) -> IoFuture<'_, Vec<u8>> {
        let input = self.input.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut buffer = vec![0; max_bytes as usize];
                let count = input
                    .lock()
                    .unwrap_or_else(|_| panic!("hosted serial stdin mutex poisoned"))
                    .read(&mut buffer)?;
                buffer.truncate(count);
                Ok(buffer)
            })
            .await
            .unwrap_or_else(|error| panic!("hosted serial stdin task panicked: {error}"))
        })
    }

    fn write(&self, bytes: Vec<u8>) -> IoFuture<'_, ()> {
        let output = self.output.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                output
                    .lock()
                    .unwrap_or_else(|_| panic!("hosted serial stdout mutex poisoned"))
                    .write_all(&bytes)?;
                Ok(())
            })
            .await
            .unwrap_or_else(|error| panic!("hosted serial stdout task panicked: {error}"))
        })
    }

    fn flush(&self) -> IoFuture<'_, ()> {
        let output = self.output.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                output
                    .lock()
                    .unwrap_or_else(|_| panic!("hosted serial stdout mutex poisoned"))
                    .flush()?;
                Ok(())
            })
            .await
            .unwrap_or_else(|error| panic!("hosted serial flush task panicked: {error}"))
        })
    }
}

#[cfg(test)]
impl HostedSerialBackend for DuplexSerialBackend {
    fn read(&self, max_bytes: u32) -> IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            #[cfg(test)]
            self.trace.reads.fetch_add(1, Ordering::Relaxed);
            let mut io = self.read.lock().await;
            let mut buffer = vec![0; max_bytes as usize];
            let count = match io.read(&mut buffer).await {
                Ok(count) => count,
                Err(error) if is_serial_disconnect(&error) => 0,
                Err(error) => return Err(error),
            };
            buffer.truncate(count);
            #[cfg(test)]
            self.trace
                .read_bytes
                .lock()
                .unwrap_or_else(|_| panic!("serial trace read-bytes mutex poisoned"))
                .push(buffer.clone());
            Ok(buffer)
        })
    }

    fn write(&self, bytes: Vec<u8>) -> IoFuture<'_, ()> {
        Box::pin(async move {
            #[cfg(test)]
            self.trace.writes.fetch_add(1, Ordering::Relaxed);
            let mut io = self.write.lock().await;
            if let Err(error) = io.write_all(&bytes).await
                && !is_serial_disconnect(&error)
            {
                return Err(error);
            }
            #[cfg(test)]
            self.trace
                .write_bytes
                .lock()
                .unwrap_or_else(|_| panic!("serial trace write-bytes mutex poisoned"))
                .push(bytes);
            Ok(())
        })
    }

    fn flush(&self) -> IoFuture<'_, ()> {
        Box::pin(async move {
            #[cfg(test)]
            self.trace.flushes.fetch_add(1, Ordering::Relaxed);
            let mut io = self.write.lock().await;
            if let Err(error) = io.flush().await
                && !is_serial_disconnect(&error)
            {
                return Err(error);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
fn is_serial_disconnect(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

fn debug_port_resource(
    store: &mut StoreData,
) -> wasmtime::Result<Option<Resource<HostedSerialPort>>> {
    match &store.serial {
        Some(io) => Ok(Some(store.table.push(HostedSerialPort { io: io.clone() })?)),
        None => Ok(None),
    }
}

fn serial_rights<T>(
    table: &mut wasmtime::component::ResourceTable,
    resource: Resource<T>,
) -> wasmtime::Result<()>
where
    T: 'static,
{
    let _ = table.get(&resource)?;
    Ok(())
}

fn serial_io(
    store: &mut StoreData,
    resource: &Resource<HostedSerialPort>,
) -> wasmtime::Result<HostedSerialIo> {
    Ok(store.table.get(resource)?.io.clone())
}

macro_rules! impl_serial_bindings {
    ($bindings:ident) => {
        impl crate::$bindings::bindings::helios::system::serial::Host for StoreData {
            async fn debug_port(&mut self) -> wasmtime::Result<Option<Resource<HostedSerialPort>>> {
                debug_port_resource(self)
            }
        }

        impl crate::$bindings::bindings::helios::system::serial::HostSerialPort for StoreData {
            async fn rights(
                &mut self,
                resource: Resource<HostedSerialPort>,
            ) -> wasmtime::Result<crate::$bindings::bindings::helios::system::serial::SerialRights>
            {
                serial_rights(&mut self.table, resource)?;
                Ok(
                    crate::$bindings::bindings::helios::system::serial::SerialRights::READ
                        | crate::$bindings::bindings::helios::system::serial::SerialRights::WRITE
                        | crate::$bindings::bindings::helios::system::serial::SerialRights::FLUSH,
                )
            }

            async fn drop(&mut self, resource: Resource<HostedSerialPort>) -> wasmtime::Result<()> {
                let _ = self.table.delete(resource)?;
                Ok(())
            }
        }

        impl crate::$bindings::bindings::helios::system::serial::HostSerialPortWithStore
            for HasSelf<StoreData>
        {
            async fn read<T: 'static + Send>(
                accessor: &Accessor<T, Self>,
                resource: Resource<HostedSerialPort>,
                max_bytes: u32,
            ) -> wasmtime::Result<Vec<u8>> {
                let io = accessor.with(|mut access| serial_io(access.get(), &resource))?;
                io.backend.read(max_bytes).await.map_err(Into::into)
            }

            async fn write<T: 'static + Send>(
                accessor: &Accessor<T, Self>,
                resource: Resource<HostedSerialPort>,
                bytes: Vec<u8>,
            ) -> wasmtime::Result<()> {
                let io = accessor.with(|mut access| serial_io(access.get(), &resource))?;
                io.backend.write(bytes).await.map_err(Into::into)
            }

            async fn flush<T: 'static + Send>(
                accessor: &Accessor<T, Self>,
                resource: Resource<HostedSerialPort>,
            ) -> wasmtime::Result<()> {
                let io = accessor.with(|mut access| serial_io(access.get(), &resource))?;
                io.backend.flush().await.map_err(Into::into)
            }
        }
    };
}

impl_serial_bindings!(program_bindings);
impl_serial_bindings!(debugger_bindings);

#[cfg(test)]
mod tests {
    use std::io;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::HostedSerialIo;

    #[test]
    fn duplex_backend_roundtrips_bytes() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| {
                panic!("failed to build tokio runtime for serial test: {error}")
            })
            .block_on(async {
                let (host, mut peer) = HostedSerialIo::duplex_pair(4096);

                host.backend
                    .write(b"ping".to_vec())
                    .await
                    .unwrap_or_else(|error| panic!("host write failed: {error}"));

                let peer_bytes = async {
                    let mut buf = [0_u8; 4];
                    peer.read_exact(&mut buf).await?;
                    io::Result::Ok(buf)
                }
                .await;
                assert_eq!(
                    &peer_bytes.unwrap_or_else(|error| panic!("{error}")),
                    b"ping"
                );

                peer.write_all(b"pong")
                    .await
                    .unwrap_or_else(|error| panic!("peer write failed: {error}"));
                let bytes = host
                    .backend
                    .read(4)
                    .await
                    .unwrap_or_else(|error| panic!("host read failed: {error}"));
                assert_eq!(bytes, b"pong");
            });
    }
}

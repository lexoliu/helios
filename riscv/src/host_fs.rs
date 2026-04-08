extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use core::num::NonZeroU32;
use fdt::Fdt;
use futures::channel::oneshot;
use helios_hal::cpu::Cpu;
use helios_hal::io::IoError;
use helios_kernel::{Kernel, Notify};
use plic::Plic;
use thiserror::Error;

use crate::RiscvCpu;
use crate::net::{InterruptSourceId, PlicContext};

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_MMIO_MODERN_VERSION: u32 = 2;
const VIRTIO_MMIO_9P_DEVICE_ID: u32 = 9;
const VIRTIO_MMIO_MAGIC_OFFSET: usize = 0x000;
const VIRTIO_MMIO_VERSION_OFFSET: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x008;
const DEFAULT_MSIZE: u32 = (128 * 1024) + 24;
const P9_NOTAG: u16 = u16::MAX;
const P9_NOFID: u32 = u32::MAX;
const P9_NOUID: u32 = u32::MAX;
const P9_TVERSION: u8 = 100;
const P9_TATTACH: u8 = 104;
const P9_TWALK: u8 = 110;
const P9_TREAD: u8 = 116;
const P9_TWRITE: u8 = 118;
const P9_TCLUNK: u8 = 120;
const P9_TLOPEN: u8 = 12;
const P9_TLCREATE: u8 = 14;
const P9_TGETATTR: u8 = 24;
const P9_TREADDIR: u8 = 40;
const P9_TMKDIR: u8 = 72;
const P9_TRENAMEAT: u8 = 74;
const P9_TUNLINKAT: u8 = 76;
const P9_RLERROR: u8 = 7;
const P9_DOTL_RDONLY: u32 = 0;
const P9_DOTL_WRONLY: u32 = 1;
const P9_DOTL_TRUNC: u32 = 0o1000;
const P9_DOTL_DIRECTORY: u32 = 0o200000;
const P9_DOTL_AT_REMOVEDIR: u32 = 0x200;
const P9_STATS_BASIC: u64 = 0x0000_07ff;
const P9_QTDIR: u8 = 0x80;
const P9_WRITE_CHUNK: usize = (DEFAULT_MSIZE as usize) - 24;

#[derive(Clone)]
pub(crate) struct HostFileSystemService {
    inner: Arc<HostFileSystemServiceInner>,
}

struct HostFileSystemServiceInner {
    device: Arc<helios_virtio::VirtioMmio9pDevice>,
    requests: ConcurrentQueue<Request>,
    ready: Notify,
}

#[derive(Clone, Debug)]
pub(crate) struct HostDirEntry {
    pub(crate) name: String,
    pub(crate) is_directory: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct HostMetadata {
    pub(crate) qid_path: u64,
    pub(crate) qid_type: u8,
    pub(crate) mode: u32,
    pub(crate) size: u64,
}

#[derive(Debug, Error)]
pub(crate) enum HostFsError {
    #[error("9p transport error: {0}")]
    Transport(#[from] IoError),
    #[error("9p protocol error: {0}")]
    Protocol(&'static str),
    #[error("9p server error code {0}")]
    Server(u32),
    #[error("9p string field is not valid utf-8")]
    Utf8,
}

pub(crate) struct HostFsInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) service: HostFileSystemService,
}

pub(crate) struct HostFsProbe {
    pub(crate) plic: &'static Plic,
    pub(crate) context: PlicContext,
    pub(crate) interrupt: HostFsInterrupt,
}

enum Request {
    Raw {
        bytes: Vec<u8>,
        response_len: usize,
        completion: oneshot::Sender<Result<Vec<u8>, IoError>>,
    },
}

pub(crate) fn install(
    cpu: &RiscvCpu,
    kernel: &Kernel<RiscvCpu>,
    fdt: &Fdt<'_>,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<HostFsProbe> {
    let Some((device, source)) = discover_9p_device(fdt) else {
        tracing::warn!("virtio 9p device was not discovered on the platform bus");
        return None;
    };
    let Some((plic, context)) =
        crate::net::discover_plic_context(fdt, cpu.bootstrap_processor().id())
    else {
        tracing::warn!("virtio 9p device was discovered but no PLIC context was available");
        return None;
    };
    let service = HostFileSystemService {
        inner: Arc::new(HostFileSystemServiceInner {
            device,
            requests: ConcurrentQueue::unbounded(),
            ready: Notify::new(),
        }),
    };
    let runner = service.clone();
    debug_state.install_host_fs_service(service.clone());
    kernel.spawn_local_detached(async move {
        runner.run().await;
    });
    Some(HostFsProbe {
        plic,
        context,
        interrupt: HostFsInterrupt { source, service },
    })
}

impl HostFileSystemService {
    pub(crate) fn mount_tag(&self) -> &str {
        self.inner.device.mount_tag()
    }

    pub(crate) fn handle_interrupt(&self) {
        self.inner.device.handle_interrupt();
    }

    pub(crate) async fn raw_request(
        &self,
        bytes: Vec<u8>,
        response_len: usize,
    ) -> Result<Vec<u8>, IoError> {
        let (completion, rx) = oneshot::channel();
        self.inner
            .requests
            .push(Request::Raw {
                bytes,
                response_len,
                completion,
            })
            .unwrap_or_else(|error| match error {
                PushError::Full(_) => unreachable!("host-fs request queue reported full"),
                PushError::Closed(_) => panic!("host-fs request queue was closed unexpectedly"),
            });
        self.inner.ready.notify_one();
        rx.await.unwrap_or_else(|_| Err(IoError::DeviceFault))
    }

    async fn run(&self) {
        loop {
            let request = self.next_request().await;
            match request {
                Request::Raw {
                    bytes,
                    response_len,
                    completion,
                } => {
                    let mut response = vec![0_u8; response_len];
                    let result = self
                        .inner
                        .device
                        .request(&bytes, &mut response)
                        .await
                        .and_then(|used| {
                            let used = usize::try_from(used).map_err(|_| IoError::DeviceFault)?;
                            response.truncate(used);
                            Ok(response)
                        });
                    let _ = completion.send(result);
                }
            }
        }
    }

    async fn next_request(&self) -> Request {
        loop {
            match self.inner.requests.pop() {
                Ok(request) => return request,
                Err(PopError::Empty) => self.inner.ready.notified().await,
                Err(PopError::Closed) => panic!("host-fs request queue was closed unexpectedly"),
            }
        }
    }

    async fn transact(
        &self,
        ty: u8,
        body: impl FnOnce(&mut Vec<u8>),
        response_len: usize,
    ) -> Result<Vec<u8>, HostFsError> {
        let mut request = Vec::with_capacity(7 + 256);
        request.extend_from_slice(&0_u32.to_le_bytes());
        request.push(ty);
        request.extend_from_slice(&P9_NOTAG.to_le_bytes());
        body(&mut request);
        let size =
            u32::try_from(request.len()).map_err(|_| HostFsError::Protocol("request too large"))?;
        request[..4].copy_from_slice(&size.to_le_bytes());

        let response = self.raw_request(request, response_len).await?;
        if response.len() < 7 {
            return Err(HostFsError::Protocol("response shorter than 9p header"));
        }
        let actual_size = read_u32_le(&response, 0)? as usize;
        if actual_size != response.len() {
            return Err(HostFsError::Protocol("response length header mismatch"));
        }

        let response_ty = response[4];
        if response_ty == P9_RLERROR {
            return Err(HostFsError::Server(read_u32_le(&response, 7)?));
        }

        let expected = ty
            .checked_add(1)
            .ok_or(HostFsError::Protocol("response type overflowed"))?;
        if response_ty != expected {
            return Err(HostFsError::Protocol("unexpected 9p response type"));
        }

        Ok(response)
    }

    pub(crate) async fn stat_path(&self, path: &str) -> Result<HostMetadata, HostFsError> {
        let fid = self.attach_root().await?;
        if path == "/" {
            let metadata = self.get_attr(fid).await;
            let _ = self.clunk(fid).await;
            return metadata;
        }
        let target = self.walk(fid, 1, path).await?;
        let metadata = self.get_attr(target).await;
        let _ = self.clunk(target).await;
        let _ = self.clunk(fid).await;
        metadata
    }

    pub(crate) async fn read_dir(&self, path: &str) -> Result<Vec<HostDirEntry>, HostFsError> {
        let fid = self.attach_root().await?;
        if path == "/" {
            let entries = self.read_dir_entries(fid).await;
            let _ = self.clunk(fid).await;
            return entries;
        }
        let target = self.walk(fid, 1, path).await?;
        let entries = self.read_dir_entries(target).await;
        let _ = self.clunk(target).await;
        let _ = self.clunk(fid).await;
        entries
    }

    pub(crate) async fn read_file(&self, path: &str) -> Result<Vec<u8>, HostFsError> {
        let fid = self.attach_root().await?;
        let target = self.walk(fid, 1, path).await?;
        let data = self.read_file_all(target).await;
        let _ = self.clunk(target).await;
        let _ = self.clunk(fid).await;
        data
    }

    pub(crate) async fn create_directory(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let root = self.attach_root().await?;
        let directory = self.walk(root, 1, parent).await?;
        let result = self
            .transact(
                P9_TMKDIR,
                |body| {
                    push_u32(body, directory);
                    push_string(body, name);
                    push_u32(body, 0o755);
                    push_u32(body, P9_NOUID);
                },
                64,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(directory).await;
        let _ = self.clunk(root).await;
        result
    }

    pub(crate) async fn create_file(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let root = self.attach_root().await?;
        let directory = self.walk(root, 1, parent).await?;
        let result = self
            .transact(
                P9_TLCREATE,
                |body| {
                    push_u32(body, directory);
                    push_string(body, name);
                    push_u32(body, P9_DOTL_WRONLY);
                    push_u32(body, 0o644);
                    push_u32(body, P9_NOUID);
                },
                64,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(directory).await;
        let _ = self.clunk(root).await;
        result
    }

    pub(crate) async fn truncate_file(&self, path: &str) -> Result<(), HostFsError> {
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        let result = self.open(file, P9_DOTL_WRONLY | P9_DOTL_TRUNC).await;
        let _ = self.clunk(file).await;
        let _ = self.clunk(root).await;
        result
    }

    pub(crate) async fn write_file(
        &self,
        path: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), HostFsError> {
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        self.open(file, P9_DOTL_WRONLY).await?;
        let result = self.write_chunks(file, offset, bytes).await;
        let _ = self.clunk(file).await;
        let _ = self.clunk(root).await;
        result
    }

    pub(crate) async fn remove(&self, path: &str, directory: bool) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let root = self.attach_root().await?;
        let dir = self.walk(root, 1, parent).await?;
        let flags = if directory { P9_DOTL_AT_REMOVEDIR } else { 0 };
        let result = self
            .transact(
                P9_TUNLINKAT,
                |body| {
                    push_u32(body, dir);
                    push_string(body, name);
                    push_u32(body, flags);
                },
                16,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(dir).await;
        let _ = self.clunk(root).await;
        result
    }

    pub(crate) async fn rename(&self, source: &str, destination: &str) -> Result<(), HostFsError> {
        let (source_parent, source_name) = split_parent_name(source)?;
        let (destination_parent, destination_name) = split_parent_name(destination)?;
        let root = self.attach_root().await?;
        let source_dir = self.walk(root, 1, source_parent).await?;
        let destination_dir = self.walk(root, 2, destination_parent).await?;
        let result = self
            .transact(
                P9_TRENAMEAT,
                |body| {
                    push_u32(body, source_dir);
                    push_string(body, source_name);
                    push_u32(body, destination_dir);
                    push_string(body, destination_name);
                },
                16,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(destination_dir).await;
        let _ = self.clunk(source_dir).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn attach_root(&self) -> Result<u32, HostFsError> {
        self.version().await?;
        let fid = 0;
        let mount_tag = self.mount_tag().to_owned();
        let _ = self
            .transact(
                P9_TATTACH,
                move |body| {
                    push_u32(body, fid);
                    push_u32(body, P9_NOFID);
                    push_string(body, "root");
                    push_string(body, &mount_tag);
                    push_u32(body, P9_NOUID);
                },
                64,
            )
            .await?;
        Ok(fid)
    }

    async fn version(&self) -> Result<(), HostFsError> {
        let _ = self
            .transact(
                P9_TVERSION,
                |body| {
                    push_u32(body, DEFAULT_MSIZE);
                    push_string(body, "9P2000.L");
                },
                64,
            )
            .await?;
        Ok(())
    }

    async fn walk(&self, parent_fid: u32, new_fid: u32, path: &str) -> Result<u32, HostFsError> {
        let segments = split_path(path);
        let response = self
            .transact(
                P9_TWALK,
                |body| {
                    push_u32(body, parent_fid);
                    push_u32(body, new_fid);
                    push_u16(
                        body,
                        u16::try_from(segments.len()).expect("walk segment count overflowed u16"),
                    );
                    for segment in &segments {
                        push_string(body, segment);
                    }
                },
                4096,
            )
            .await?;
        let cursor = 7;
        let walked = read_u16_le(&response, cursor)?;
        if usize::from(walked) != segments.len() {
            return Err(HostFsError::Protocol("walk did not resolve the full path"));
        }
        Ok(new_fid)
    }

    async fn get_attr(&self, fid: u32) -> Result<HostMetadata, HostFsError> {
        let response = self
            .transact(
                P9_TGETATTR,
                |body| {
                    push_u32(body, fid);
                    push_u64(body, P9_STATS_BASIC);
                },
                160,
            )
            .await?;
        let mut cursor = 7;
        let _mask = read_u64_le(&response, cursor)?;
        cursor += 8;
        let qid_type = read_u8(&response, cursor)?;
        cursor += 1;
        let _qid_version = read_u32_le(&response, cursor)?;
        cursor += 4;
        let qid_path = read_u64_le(&response, cursor)?;
        cursor += 8;
        let mode = read_u32_le(&response, cursor)?;
        cursor += 4;
        cursor += 4;
        cursor += 4;
        cursor += 8;
        cursor += 8;
        let size = read_u64_le(&response, cursor)?;
        Ok(HostMetadata {
            qid_path,
            qid_type,
            mode,
            size,
        })
    }

    async fn read_file_all(&self, fid: u32) -> Result<Vec<u8>, HostFsError> {
        self.open(fid, P9_DOTL_RDONLY).await?;

        let mut bytes = Vec::new();
        let mut offset = 0_u64;
        loop {
            let response = self
                .transact(
                    P9_TREAD,
                    |body| {
                        push_u32(body, fid);
                        push_u64(body, offset);
                        push_u32(body, DEFAULT_MSIZE.saturating_sub(24));
                    },
                    usize::try_from(DEFAULT_MSIZE).expect("DEFAULT_MSIZE overflowed usize"),
                )
                .await?;
            let mut cursor = 7;
            let count = usize::try_from(read_u32_le(&response, cursor)?)
                .map_err(|_| HostFsError::Protocol("read payload count overflowed usize"))?;
            cursor += 4;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(read_slice(&response, cursor, count)?);
            offset = offset.saturating_add(u64::try_from(count).expect("count overflowed u64"));
        }
        Ok(bytes)
    }

    async fn read_dir_entries(&self, fid: u32) -> Result<Vec<HostDirEntry>, HostFsError> {
        self.open(fid, P9_DOTL_RDONLY | P9_DOTL_DIRECTORY).await?;

        let mut entries = Vec::new();
        let mut offset = 0_u64;
        loop {
            let response = self
                .transact(
                    P9_TREADDIR,
                    |body| {
                        push_u32(body, fid);
                        push_u64(body, offset);
                        push_u32(body, DEFAULT_MSIZE.saturating_sub(24));
                    },
                    usize::try_from(DEFAULT_MSIZE).expect("DEFAULT_MSIZE overflowed usize"),
                )
                .await?;
            let mut cursor = 7;
            let count = usize::try_from(read_u32_le(&response, cursor)?)
                .map_err(|_| HostFsError::Protocol("readdir payload count overflowed usize"))?;
            cursor += 4;
            if count == 0 {
                break;
            }
            let end = cursor + count;
            while cursor < end {
                let qid_type = read_u8(&response, cursor)?;
                cursor += 1;
                cursor += 4;
                cursor += 8;
                offset = read_u64_le(&response, cursor)?;
                cursor += 8;
                let type_ = read_u8(&response, cursor)?;
                cursor += 1;
                let name = read_string(&response, &mut cursor)?;
                if name == "." || name == ".." {
                    continue;
                }
                entries.push(HostDirEntry {
                    name,
                    is_directory: qid_type & P9_QTDIR != 0 || type_ == 4,
                });
            }
        }
        Ok(entries)
    }

    async fn clunk(&self, fid: u32) -> Result<(), HostFsError> {
        let _ = self
            .transact(P9_TCLUNK, |body| push_u32(body, fid), 16)
            .await?;
        Ok(())
    }

    async fn open(&self, fid: u32, flags: u32) -> Result<(), HostFsError> {
        let _ = self
            .transact(
                P9_TLOPEN,
                |body| {
                    push_u32(body, fid);
                    push_u32(body, flags);
                },
                64,
            )
            .await?;
        Ok(())
    }

    async fn write_chunks(
        &self,
        fid: u32,
        mut offset: u64,
        mut bytes: &[u8],
    ) -> Result<(), HostFsError> {
        while !bytes.is_empty() {
            let count = bytes.len().min(P9_WRITE_CHUNK);
            let chunk = &bytes[..count];
            let response = self
                .transact(
                    P9_TWRITE,
                    |body| {
                        push_u32(body, fid);
                        push_u64(body, offset);
                        push_u32(
                            body,
                            u32::try_from(count).expect("write chunk length overflowed u32"),
                        );
                        body.extend_from_slice(chunk);
                    },
                    32,
                )
                .await?;
            let written = usize::try_from(read_u32_le(&response, 7)?)
                .map_err(|_| HostFsError::Protocol("write count overflowed usize"))?;
            if written != count {
                return Err(HostFsError::Protocol("short 9p write"));
            }
            offset = offset
                .checked_add(u64::try_from(written).expect("written length overflowed u64"))
                .ok_or(HostFsError::Protocol("write offset overflowed u64"))?;
            bytes = &bytes[written..];
        }
        Ok(())
    }
}

fn split_path(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn split_parent_name(path: &str) -> Result<(&str, &str), HostFsError> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err(HostFsError::Transport(IoError::PermissionDenied));
    }
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some(parts) => parts,
        None => ("/", trimmed),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err(HostFsError::Transport(IoError::PermissionDenied));
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok((parent, name))
}

fn push_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_string(buf: &mut Vec<u8>, value: &str) {
    let len = u16::try_from(value.len()).expect("9p string field exceeded u16::MAX");
    push_u16(buf, len);
    buf.extend_from_slice(value.as_bytes());
}

fn read_u8(buf: &[u8], offset: usize) -> Result<u8, HostFsError> {
    buf.get(offset)
        .copied()
        .ok_or(HostFsError::Protocol("buffer underrun"))
}

fn read_u16_le(buf: &[u8], offset: usize) -> Result<u16, HostFsError> {
    let bytes = read_slice(buf, offset, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_le(buf: &[u8], offset: usize) -> Result<u32, HostFsError> {
    let bytes = read_slice(buf, offset, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64_le(buf: &[u8], offset: usize) -> Result<u64, HostFsError> {
    let bytes = read_slice(buf, offset, 8)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn read_slice<'a>(buf: &'a [u8], offset: usize, len: usize) -> Result<&'a [u8], HostFsError> {
    buf.get(offset..offset + len)
        .ok_or(HostFsError::Protocol("buffer underrun"))
}

fn read_string(buf: &[u8], cursor: &mut usize) -> Result<String, HostFsError> {
    let len = usize::from(read_u16_le(buf, *cursor)?);
    *cursor += 2;
    let bytes = read_slice(buf, *cursor, len)?;
    *cursor += len;
    String::from_utf8(bytes.to_vec()).map_err(|_| HostFsError::Utf8)
}

fn discover_9p_device(
    fdt: &Fdt<'_>,
) -> Option<(Arc<helios_virtio::VirtioMmio9pDevice>, InterruptSourceId)> {
    for node in fdt.all_nodes() {
        if !node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
        {
            continue;
        }

        let Some(region) = node.reg().and_then(|mut regs| regs.next()) else {
            continue;
        };
        let base = region.starting_address as usize;
        if !is_9p_mmio_device(base) {
            continue;
        }

        let header = core::ptr::NonNull::new(base as *mut u8)
            .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
        let mmio_size = region.size.unwrap();
        let irq_source = node
            .interrupts()
            .and_then(|mut interrupts| interrupts.next())
            .and_then(|irq| NonZeroU32::new(irq as u32))
            .map(InterruptSourceId)
            .unwrap_or_else(|| panic!("virtio-9p node at {base:#x} has no valid interrupt source"));
        let device =
            unsafe { helios_virtio::p9_from_mmio(header, mmio_size) }.unwrap_or_else(|error| {
                panic!("failed to initialize virtio-9p device at {base:#x}: {error}")
            });
        return Some((Arc::new(device), irq_source));
    }

    None
}

fn is_9p_mmio_device(base: usize) -> bool {
    let magic = unsafe { read_u32(base + VIRTIO_MMIO_MAGIC_OFFSET) };
    let version = unsafe { read_u32(base + VIRTIO_MMIO_VERSION_OFFSET) };
    let device_id = unsafe { read_u32(base + VIRTIO_MMIO_DEVICE_ID_OFFSET) };
    let matches = magic == VIRTIO_MMIO_MAGIC
        && version == VIRTIO_MMIO_MODERN_VERSION
        && device_id == VIRTIO_MMIO_9P_DEVICE_ID;
    matches
}

unsafe fn read_u32(addr: usize) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
}

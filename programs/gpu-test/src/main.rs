//! `gpu-test`: drives the machine's 3D engine and reports what it did.
//!
//! It is the guest side of the 3D path's acceptance evidence. It opens
//! the engine, reads the device's capability sets, opens a two-ring
//! virgl context, retires a fence on each ring, makes a host blob the
//! renderer allocates and a guest blob of its own pages, writes both
//! and reads them back, and then starts a second copy of itself that
//! opens the engine and retires a fence while this one still holds its
//! context. Every step prints a line, because a host that sees nothing
//! has to be told apart from a program that never got as far as
//! opening.
//!
//! On a machine whose display engine renders nothing the open is
//! refused `no-renderer` (or `unavailable`, with no display engine at
//! all) and the program prints `gpu-test:3d=none` and exits
//! successfully: that is the expected answer of the smoke lane, whose
//! QEMU offers a 2D device only. A boot that asked for a renderer passes
//! `--expect-renderer`, and the same refusal is then a failure.
//!
//! The streams are virgl's, because virgl is the one renderer every
//! `virtio-gpu-gl` host carries and whose protocol has a no-op: a
//! command buffer of zeros is a run of `VIRGL_CCMD_NOP`s, which is what
//! the fenced submissions carry. The host blob is made the way Mesa's
//! virgl winsys makes one (`virgl_drm_winsys_resource_create_blob` in
//! `src/gallium/winsys/virgl/drm/virgl_drm_winsys.c`): a
//! `VIRGL_CCMD_PIPE_RESOURCE_CREATE` naming a blob id, then a host-3D
//! blob create naming the same id.

use std::env;

use helios_api::gpu::{BlobMemory, BlobUsage, CapsetInfo, Context, Gpu, GpuError};
use helios_api::programs::{Child, DeviceClass, SpawnError, SpawnRequest, device_grant};
use helios_api::wit_bindgen::StreamReader;
use thiserror::Error;

/// `VIRTIO_GPU_CAPSET_VIRGL2` and `VIRTIO_GPU_CAPSET_VIRGL`, from the
/// virtio specification's capability-set table (5.7.6.4), newest first:
/// a host that publishes both speaks the newer protocol on the same
/// renderer.
const VIRGL_CAPSETS: [u32; 2] = [2, 1];

/// The rings the context is opened with: ring 0, the CPU timeline, and
/// one more, so a fence is retired on a ring of each kind.
const RINGS: u8 = 2;

/// The fence every submission carries: the first point on each ring.
const FENCE: u64 = 1;

/// The size of the command buffer and of both blobs: one page.
const PAGE_BYTES: u64 = 4096;

/// The renderer's name for the host blob's allocation. Any non-zero id
/// this context has not used names a fresh one; Mesa counts its own up
/// from one.
const HOST_BLOB_ID: u32 = 1;

/// The path the boot provisions this program at: every boot program is
/// installed under `/bin` by its own name.
const SELF_PATH: &str = "/bin/gpu-test";

/// What the second instance prints once its fence has retired, and what
/// the first looks for in its output.
const CHILD_DONE: &str = "gpu-test:child fence=1";

#[derive(Debug, Error)]
enum GpuTestError {
    #[error("the 3D engine refused: {0}")]
    Gpu(#[from] GpuError),
    #[error("usage: gpu-test [--expect-renderer] [--child]; `{0}` is not an option")]
    Usage(String),
    #[error("the boot asked for a renderer and the machine has none: {0}")]
    NoRenderer(GpuError),
    #[error("the device publishes no virgl capability set to open a context on")]
    NoVirgl,
    #[error("ring {ring}'s fence stream closed before fence {expected} retired")]
    FenceStreamClosed { ring: u8, expected: u64 },
    #[error("the {kind} blob read back {got:#04x} at byte {at}, not the {expected:#04x} written")]
    ReadBack {
        kind: &'static str,
        at: usize,
        expected: u8,
        got: u8,
    },
    #[error("a guest blob has pages of its own, and this one has none")]
    NoGuestPages,
    #[error("the second instance could not be started: {0:?}")]
    Spawn(SpawnError),
    #[error("the second instance exited {code} without retiring its fence")]
    Child { code: u32 },
}

/// What this run was asked to do.
struct Options {
    /// The boot asked for a renderer, so a machine without one fails.
    expect_renderer: bool,
    /// This is the second instance, started by the first.
    child: bool,
}

impl Options {
    fn parse() -> Result<Self, GpuTestError> {
        let mut options = Self {
            expect_renderer: false,
            child: false,
        };
        for argument in env::args().skip(1) {
            match argument.as_str() {
                "--expect-renderer" => options.expect_renderer = true,
                "--child" => options.child = true,
                _ => return Err(GpuTestError::Usage(argument)),
            }
        }
        Ok(options)
    }
}

#[helios_api::main]
async fn main() -> Result<(), GpuTestError> {
    let options = Options::parse()?;
    let gpu = match Gpu::open() {
        Ok(gpu) => gpu,
        Err(error @ (GpuError::NoRenderer | GpuError::Unavailable)) => {
            if options.expect_renderer || options.child {
                return Err(GpuTestError::NoRenderer(error));
            }
            println!("gpu-test:3d=none");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if options.child {
        return child(&gpu).await;
    }
    println!("gpu-test:opened");

    let capsets = gpu.capsets().await?;
    println!("gpu-test:capsets={}", capsets.len());
    let virgl = virgl_capset(&capsets)?;
    let bytes = gpu.capset(virgl.id, virgl.max_version).await?;
    println!(
        "gpu-test:capset id={} version={} bytes={}",
        virgl.id,
        virgl.max_version,
        bytes.len()
    );

    let context = gpu.create_context(virgl.id, RINGS, "gpu-test").await?;
    println!("gpu-test:context capset={} rings={RINGS}", virgl.id);

    fences_on_every_ring(&context).await?;
    host_blob(&gpu, &context).await?;
    guest_blob(&gpu, &context).await?;
    second_instance().await?;
    Ok(())
}

/// The newest virgl capability set the device publishes.
fn virgl_capset(capsets: &[CapsetInfo]) -> Result<CapsetInfo, GpuTestError> {
    VIRGL_CAPSETS
        .iter()
        .find_map(|id| capsets.iter().find(|info| info.id == *id))
        .copied()
        .ok_or(GpuTestError::NoVirgl)
}

/// Submit one page of no-ops on every ring of `context`, fenced, and
/// wait for each ring's fence.
async fn fences_on_every_ring(context: &Context) -> Result<(), GpuTestError> {
    let mut buffer = context.commands(PAGE_BYTES).await?;
    let span = buffer.span();
    println!(
        "gpu-test:commands offset={:#x} length={}",
        span.offset, span.length
    );
    // Zeros are `VIRGL_CCMD_NOP` with no payload, one per dword
    // (`enum virgl_context_cmd` in virgl_protocol.h), cleared with one
    // bulk `memory.fill` over the pinned pages.
    buffer.bytes().fill(0);

    // Every stream is opened before its submission: a fence retired
    // before the stream existed would not be what it reports first.
    let mut streams = Vec::with_capacity(usize::from(RINGS));
    for ring in 0..RINGS {
        streams.push(context.fences(ring)?);
    }
    for ring in 0..RINGS {
        context
            .submit(&buffer, 0, span.length, ring, Some(FENCE))
            .await?;
        println!("gpu-test:submit ring={ring} fence={FENCE}");
    }
    for (ring, stream) in (0..RINGS).zip(&mut streams) {
        let fence = retired(stream, ring, FENCE).await?;
        println!("gpu-test:fence ring={ring} fence={fence}");
    }
    Ok(())
}

/// Read `stream` until it reports `expected` or a later fence.
async fn retired(
    stream: &mut StreamReader<u64>,
    ring: u8,
    expected: u64,
) -> Result<u64, GpuTestError> {
    loop {
        let (result, fences) = stream.read(Vec::with_capacity(1)).await;
        if let Some(&fence) = fences.iter().find(|fence| **fence >= expected) {
            return Ok(fence);
        }
        if helios_api::stream_closed(result) {
            return Err(GpuTestError::FenceStreamClosed { ring, expected });
        }
    }
}

/// `VIRGL_CCMD_PIPE_RESOURCE_CREATE` for a one-page buffer the CPU maps
/// persistently and coherently, naming its allocation `blob_id`, as the
/// little-endian dwords the renderer decodes.
///
/// The layout is virgl_protocol.h's: the header is `VIRGL_CMD0(cmd, 0,
/// VIRGL_PIPE_RES_CREATE_SIZE)`, then the eleven fields in the order of
/// the `VIRGL_PIPE_RES_CREATE_*` indices.
fn pipe_resource_create(blob_id: u32) -> Vec<u8> {
    /// `VIRGL_CCMD_PIPE_RESOURCE_CREATE`: the 49th member of
    /// `enum virgl_context_cmd`, counted from `VIRGL_CCMD_NOP = 0`.
    const VIRGL_CCMD_PIPE_RESOURCE_CREATE: u32 = 48;
    /// `VIRGL_PIPE_RES_CREATE_SIZE`: the payload's dword count.
    const VIRGL_PIPE_RES_CREATE_SIZE: u32 = 11;
    /// `PIPE_BUFFER`, the first member of gallium's
    /// `enum pipe_texture_target` (p_defines.h).
    const PIPE_BUFFER: u32 = 0;
    /// `VIRGL_FORMAT_R8_UNORM` (virgl_hw.h): a buffer is a run of bytes.
    const VIRGL_FORMAT_R8_UNORM: u32 = 64;
    /// `VIRGL_BIND_VERTEX_BUFFER` (virgl_hw.h): the plainest binding a
    /// buffer carries; the renderer only needs one to allocate it.
    const VIRGL_BIND_VERTEX_BUFFER: u32 = 1 << 4;
    /// `VIRGL_RESOURCE_FLAG_MAP_PERSISTENT | VIRGL_RESOURCE_FLAG_MAP_COHERENT`
    /// (virgl_hw.h): what makes Mesa create a mappable blob at all.
    const MAP_PERSISTENT_COHERENT: u32 = (1 << 1) | (1 << 2);

    let header = VIRGL_CCMD_PIPE_RESOURCE_CREATE | (VIRGL_PIPE_RES_CREATE_SIZE << 16);
    let fields: [u32; 12] = [
        header,
        PIPE_BUFFER,
        VIRGL_FORMAT_R8_UNORM,
        VIRGL_BIND_VERTEX_BUFFER,
        // Width, height, depth and array size: a buffer is its byte
        // length by one by one, one layer.
        PAGE_BYTES as u32,
        1,
        1,
        1,
        // Last mip level and sample count: neither applies to a buffer.
        0,
        0,
        MAP_PERSISTENT_COHERENT,
        blob_id,
    ];
    fields
        .iter()
        .flat_map(|field| field.to_le_bytes())
        .collect()
}

/// The byte written at `at` of a blob: a pattern no zeroed page and no
/// page shifted by a byte matches.
const fn pattern(at: usize) -> u8 {
    (at as u8).wrapping_mul(31).wrapping_add(7)
}

/// Write the pattern over `bytes` and read it back.
fn write_and_read_back(kind: &'static str, bytes: &mut [u8]) -> Result<(), GpuTestError> {
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = pattern(at);
    }
    match bytes
        .iter()
        .enumerate()
        .find(|(at, byte)| **byte != pattern(*at))
    {
        Some((at, &got)) => Err(GpuTestError::ReadBack {
            kind,
            at,
            expected: pattern(at),
            got,
        }),
        None => Ok(()),
    }
}

/// A blob the renderer allocates, placed in the aperture and written
/// through it.
async fn host_blob(gpu: &Gpu, context: &Context) -> Result<(), GpuTestError> {
    context
        .execute(&pipe_resource_create(HOST_BLOB_ID), 0, None)
        .await?;
    let mut blob = gpu
        .create_blob(
            context,
            BlobMemory::Host3d,
            BlobUsage::MAPPABLE,
            PAGE_BYTES,
            u64::from(HOST_BLOB_ID),
        )
        .await?;
    println!("gpu-test:host-blob resource={}", blob.resource_id());
    context.attach(&blob).await?;
    let bytes = blob.map().await?;
    println!("gpu-test:host-blob mapped bytes={}", bytes.len());
    write_and_read_back("host", bytes)?;
    blob.unmap().await?;
    context.detach(&blob).await?;
    println!("gpu-test:host-blob ok");
    Ok(())
}

/// A blob whose storage is this program's own pinned pages.
async fn guest_blob(gpu: &Gpu, context: &Context) -> Result<(), GpuTestError> {
    let mut blob = gpu
        .create_blob(
            context,
            BlobMemory::Guest,
            BlobUsage::empty(),
            PAGE_BYTES,
            0,
        )
        .await?;
    println!("gpu-test:guest-blob resource={}", blob.resource_id());
    context.attach(&blob).await?;
    write_and_read_back("guest", blob.bytes().ok_or(GpuTestError::NoGuestPages)?)?;
    context.detach(&blob).await?;
    println!("gpu-test:guest-blob ok");
    Ok(())
}

/// Start a second instance of this program and wait for it to retire a
/// fence of its own while this one still holds its context.
async fn second_instance() -> Result<(), GpuTestError> {
    let child = Child::spawn(SpawnRequest {
        name: String::from("gpu-test"),
        args: vec![String::from("--child")],
        env: Vec::new(),
        path: String::from(SELF_PATH),
        capability_grants: vec![device_grant(vec![DeviceClass::Gpu])],
    })
    .await
    .map_err(GpuTestError::Spawn)?;
    let output = child.read_stdout().await;
    let status = child.wait().await.map_err(GpuTestError::Spawn)?;
    let output = String::from_utf8_lossy(&output);
    print!("{output}");
    if status.code != 0 || !output.lines().any(|line| line == CHILD_DONE) {
        return Err(GpuTestError::Child { code: status.code });
    }
    println!("gpu-test:instances=2");
    Ok(())
}

/// The second instance: one context, one fenced no-op, one fence.
async fn child(gpu: &Gpu) -> Result<(), GpuTestError> {
    let capsets = gpu.capsets().await?;
    let virgl = virgl_capset(&capsets)?;
    let context = gpu.create_context(virgl.id, 1, "gpu-test-child").await?;
    let mut buffer = context.commands(PAGE_BYTES).await?;
    let length = buffer.span().length;
    buffer.bytes().fill(0);
    let mut fences = context.fences(0)?;
    context.submit(&buffer, 0, length, 0, Some(FENCE)).await?;
    retired(&mut fences, 0, FENCE).await?;
    println!("{CHILD_DONE}");
    Ok(())
}

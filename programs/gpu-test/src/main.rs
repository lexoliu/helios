//! `gpu-test`: drives the machine's 3D engine and reports what it did.
//!
//! It is the guest side of the 3D path's acceptance evidence. It claims
//! the engine, reads the device's capability sets, opens a renderer
//! context on the first one the device names — virgl, Venus or
//! gfxstream, whichever the host put there — pins a command buffer of
//! empty commands, submits it with a fence, and prints the fence the
//! device retired. Every step prints a line, because a host that sees
//! nothing has to be told apart from a program that never got as far as
//! claiming.
//!
//! On a machine whose display engine renders nothing, the claim is
//! refused `no-renderer` and the program prints `gpu-test:3d=none` and
//! exits successfully: that is the expected answer of the smoke lane,
//! whose QEMU offers a 2D device only.
//!
//! The command stream is empty on purpose. The kernel carries the bytes
//! without reading them, so what this program proves is the plumbing —
//! claim, capset, context, pinned pages, submission, fence — rather
//! than a renderer's parser, which a smoke lane has no renderer to run
//! anyway.

use helios_api::gpu::{Gpu, GpuError};
use thiserror::Error;

/// The fence the submission carries.
const SUBMIT_FENCE: u64 = 1;

/// The size of the pinned command buffer: one page of command stream.
const COMMAND_BUFFER_BYTES: u64 = 4096;

#[derive(Debug, Error)]
enum GpuTestError {
    #[error("the 3D engine refused: {0}")]
    Gpu(#[from] GpuError),
    #[error("the device reported no capability sets to negotiate a context on")]
    NoCapsets,
    #[error("the context's fence stream closed before the submitted fence retired")]
    FenceStreamClosed,
    #[error("the context's fence stream retired fence {got}, not the submitted fence {expected}")]
    WrongFence { expected: u64, got: u64 },
}

#[helios_api::main]
async fn main() -> Result<(), GpuTestError> {
    let gpu = match Gpu::claim() {
        Ok(gpu) => gpu,
        Err(GpuError::NoRenderer | GpuError::Unavailable) => {
            println!("gpu-test:3d=none");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    println!("gpu-test:claimed");

    let capsets = gpu.capsets().await?;
    println!("gpu-test:capsets={}", capsets.len());
    let chosen = *capsets.first().ok_or(GpuTestError::NoCapsets)?;
    let bytes = gpu.capset(chosen.id).await?;
    println!("gpu-test:capset id={} bytes={}", chosen.id, bytes.len());

    let context = gpu.create_context(chosen.id, "gpu-test").await?;
    println!("gpu-test:context capset={}", chosen.id);

    let buffer = context.commands(COMMAND_BUFFER_BYTES).await?;
    let placement = buffer.placement();
    println!(
        "gpu-test:commands offset={:#x} length={}",
        placement.offset, placement.length
    );

    // The stream is armed before the submission: the fence is published
    // the moment the device has taken the buffer, and a reader that
    // asked afterwards could arrive after the answer it was waiting for.
    let mut fences = context.fences();
    match context
        .submit(&buffer, 0, placement.length, SUBMIT_FENCE)
        .await
    {
        Ok(()) => println!("gpu-test:submit ok"),
        // A refused submission still retires its fence, so the stream
        // read below has an answer to give either way.
        Err(error) => println!("gpu-test:submit refused {error}"),
    }

    let (result, retired) = fences.read(Vec::with_capacity(1)).await;
    let Some(&fence) = retired.first() else {
        if helios_api::stream_closed(result) {
            return Err(GpuTestError::FenceStreamClosed);
        }
        return Err(GpuTestError::FenceStreamClosed);
    };
    if fence != SUBMIT_FENCE {
        return Err(GpuTestError::WrongFence {
            expected: SUBMIT_FENCE,
            got: fence,
        });
    }
    println!("gpu-test:fence={fence}");
    Ok(())
}

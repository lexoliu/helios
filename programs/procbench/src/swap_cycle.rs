//! `procbench swap-cycle`: proves that swap returns every byte of an
//! evicted instance.
//!
//! The parent spawns a child of the same program in the `swap-child`
//! role. The child fills a region with a per-page pattern and blocks on
//! its stdin. The parent waits out the idle threshold, then holds a
//! growing allocation that it keeps touching, so the child is the only
//! eviction candidate, until the child reports no resident bytes of its
//! own (`child.resident-bytes`; the machine-wide swap-out counter also
//! counts other instances' pages and the child's pages outside the
//! region, so it only feeds the deadline and the metrics). It then wakes
//! the child, which verifies every byte, and requires the swap-in
//! counter to cover the region's pages as well.

use std::hint::black_box;
use std::io::{self, Read as _, Write as _};
use std::time::{Duration, Instant};

use helios_api::wit_bindgen::StreamReader;
use helios_api::{stats, task};
use helios_bench_metrics::report_metric;

use crate::{ChildSpec, ProcbenchError, READ_CHUNK_BYTES, wait_child};

/// The kernel's page size; the unit swap moves and the counters count.
const PAGE_BYTES: usize = 4096;
const PAGES_PER_MIB: u64 = (1024 * 1024 / PAGE_BYTES) as u64;

/// How long an instance has to have been off every processor before the
/// swap policy evicts it: `IDLE_SWAP_AFTER` in `kernel/src/memory/swap.rs`.
const IDLE_SWAP_AFTER: Duration = Duration::from_secs(10);

/// How often the swap policy runs: `SWAP_TICK` in
/// `kernel/src/memory/swap.rs`. The hog loop runs once per tick.
const SWAP_TICK: Duration = Duration::from_millis(500);

/// Bytes one swap pass moves: `SWAP_BATCH_BYTES` in
/// `kernel/src/memory/swap.rs`.
const SWAP_BATCH_BYTES: u64 = 2 * 1024 * 1024;

/// The deadline allows this many times the time the policy needs at one
/// batch per tick.
const DEADLINE_FACTOR: u32 = 4;

/// Bytes the hog grows by when swap has not started moving pages.
///
/// The Yellow band is a quarter of the user pool, per
/// `PressureLevel::from_free_fraction` in `kernel/src/exec/compaction.rs`
/// (free fraction 25 % to 50 %), so a step this size cannot cross it
/// within one tick.
const HOG_STEP_BYTES: usize = 8 * 1024 * 1024;

/// Defaults of the options the usage line documents.
const DEFAULT_MIB: u64 = 64;
const DEFAULT_SEED: u64 = 0x5eed_5eed_5eed_5eed;

/// The child's first stdout line is `filled=<pages>`; its last is
/// `verified=<pages>`.
const FILLED_PREFIX: &str = "filled=";
const VERIFIED_PREFIX: &str = "verified=";

/// The parent's options.
pub(crate) struct Options {
    mib: u64,
    seed: u64,
    corrupt_page: Option<u64>,
}

impl Options {
    /// Parses `[--mib <n>] [--seed <n>] [--corrupt-page <index>]`, and
    /// leaves the iterator at the first argument that is not an option.
    pub(crate) fn parse(
        args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    ) -> Result<Self, ProcbenchError> {
        let mut options = Self {
            mib: DEFAULT_MIB,
            seed: DEFAULT_SEED,
            corrupt_page: None,
        };
        while let Some(flag) = args.next_if(|argument| argument.starts_with("--")) {
            let value = parse_u64(args.next())?;
            match flag.as_str() {
                "--mib" if value > 0 => options.mib = value,
                "--seed" => options.seed = value,
                "--corrupt-page" => options.corrupt_page = Some(value),
                _ => return Err(ProcbenchError::Usage),
            }
        }
        if let Some(page) = options.corrupt_page {
            let pages = options.mib * PAGES_PER_MIB;
            if page >= pages {
                return Err(ProcbenchError::CorruptPageOutOfRange { page, pages });
            }
        }
        Ok(options)
    }
}

fn parse_u64(raw: Option<String>) -> Result<u64, ProcbenchError> {
    let raw = raw.ok_or(ProcbenchError::Usage)?;
    raw.parse::<u64>()
        .map_err(|source| ProcbenchError::InvalidNumber { raw, source })
}

/// SplitMix64, the generator the per-page pattern is expanded from.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Writes page `index`'s pattern: SplitMix64 seeded with `seed ^ index`,
/// laid out as little-endian words.
fn fill_page(seed: u64, index: u64, page: &mut [u8]) {
    let mut generator = SplitMix64(seed ^ index);
    for word in page.as_chunks_mut::<{ size_of::<u64>() }>().0 {
        word.copy_from_slice(&generator.next().to_le_bytes());
    }
}

/// The swap counters at one instant.
#[derive(Clone, Copy)]
struct Counters {
    pages_out: u64,
    pages_in: u64,
}

fn swap_counters() -> Result<Counters, ProcbenchError> {
    let swap = stats::snapshot()
        .swap
        .ok_or(ProcbenchError::SwapUnavailable)?;
    Ok(Counters {
        pages_out: swap.pages_out,
        pages_in: swap.pages_in,
    })
}

/// A buffer the parent holds and keeps touching, so it is never idle.
struct Hog {
    chunks: Vec<Vec<u8>>,
    epoch: u8,
}

impl Hog {
    fn grow(&mut self) -> Result<(), ProcbenchError> {
        let mut chunk = Vec::new();
        chunk
            .try_reserve_exact(HOG_STEP_BYTES)
            .map_err(|_| ProcbenchError::HogAllocation {
                bytes: HOG_STEP_BYTES,
            })?;
        // Resizing writes every page, which is what makes it resident.
        chunk.resize(HOG_STEP_BYTES, self.epoch);
        self.chunks.push(chunk);
        Ok(())
    }

    /// Writes one byte of every page, yielding between chunks so the
    /// executor is never pinned.
    async fn touch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        for chunk in &mut self.chunks {
            for page in chunk.chunks_mut(PAGE_BYTES) {
                page[0] = self.epoch;
            }
            black_box(&*chunk);
            task::yield_now().await;
        }
    }
}

/// Reads the child's stdout into `output` until `until` holds for it, or
/// the stream closes, which reports whether `until` was reached.
async fn read_stdout(
    stdout: &mut StreamReader<u8>,
    output: &mut Vec<u8>,
    until: impl Fn(&[u8]) -> bool,
) -> bool {
    while !until(output) {
        let (result, chunk) = stdout.read(Vec::with_capacity(READ_CHUNK_BYTES)).await;
        output.extend_from_slice(&chunk);
        if chunk.is_empty() && helios_api::stream_closed(result) {
            return until(output);
        }
    }
    true
}

fn has_line(output: &[u8]) -> bool {
    output.contains(&b'\n')
}

pub(crate) async fn swap_cycle(options: Options, child: ChildSpec) -> Result<(), ProcbenchError> {
    let child_pages = options.mib * PAGES_PER_MIB;
    let baseline = swap_counters()?;

    let child = ChildSpec {
        path: child.path,
        args: vec![
            "swap-child".to_owned(),
            options.mib.to_string(),
            options.seed.to_string(),
            options
                .corrupt_page
                .map_or_else(|| "-".to_owned(), |page| page.to_string()),
        ],
    };
    let handle = child.spawn().await?;
    let (mut stdout, _completion) = handle.stdout();
    let mut output = Vec::new();

    if !read_stdout(&mut stdout, &mut output, has_line).await {
        wait_child(handle, &child.path).await?;
        return Err(ProcbenchError::NoOutput { path: child.path });
    }
    let expected = format!("{FILLED_PREFIX}{child_pages}");
    if !String::from_utf8_lossy(&output).starts_with(&expected) {
        return Err(ProcbenchError::ChildReport {
            expected,
            output: String::from_utf8_lossy(&output).trim().to_owned(),
        });
    }

    // The deadline is four times what the policy needs to move the child's
    // bytes at one batch per tick, counted from the first page that leaves
    // memory. How long the hog takes to reach that point depends on the
    // size of the machine, so that phase is bounded by the memory floor
    // below and not by time.
    let batches = (child_pages * PAGE_BYTES as u64).div_ceil(SWAP_BATCH_BYTES);
    let expected_eviction = SWAP_TICK * u32::try_from(batches).expect("batch count fits u32");
    let deadline = DEADLINE_FACTOR * expected_eviction;
    let mut eviction_started: Option<Instant> = None;
    task::sleep(IDLE_SWAP_AFTER).await;

    let mut hog = Hog {
        chunks: Vec::new(),
        epoch: 0,
    };
    let mut previous = baseline.pages_out;
    loop {
        hog.touch().await;
        let sample = stats::snapshot();
        let swap = sample.swap.ok_or(ProcbenchError::SwapUnavailable)?;
        // The machine-wide counter also moves for the parent's and other
        // instances' pages, so only the child's own figure ends the wait.
        let resident_bytes = handle.resident_bytes();
        if resident_bytes == 0 {
            break;
        }
        let evicted = swap.pages_out.saturating_sub(baseline.pages_out);
        // A fail-fast guard against driving the machine into the OOM band
        // when swap never starts; it is not an estimate of pressure.
        if swap.pages_out == baseline.pages_out
            && sample.memory.available_bytes.saturating_mul(8) < sample.memory.total_bytes
        {
            return Err(ProcbenchError::MemoryFloor {
                available_bytes: sample.memory.available_bytes,
                total_bytes: sample.memory.total_bytes,
                pages_out: swap.pages_out,
                baseline_pages_out: baseline.pages_out,
            });
        }
        if evicted != 0 && eviction_started.is_none() {
            eviction_started = Some(Instant::now());
        }
        if let Some(started) = eviction_started
            && started.elapsed() > deadline
        {
            return Err(ProcbenchError::SwapOutDeadline {
                waited: started.elapsed(),
                resident_bytes,
                pages_out: swap.pages_out,
                baseline_pages_out: baseline.pages_out,
                child_pages,
                hog_bytes: hog.chunks.len() * HOG_STEP_BYTES,
            });
        }
        if swap.pages_out == previous {
            hog.grow()?;
        }
        previous = swap.pages_out;
        task::sleep(SWAP_TICK).await;
    }

    handle.write_stdin(vec![b'\n']).await?;
    read_stdout(&mut stdout, &mut output, |_| false).await;
    wait_child(handle, &child.path).await?;

    let text = String::from_utf8_lossy(&output);
    let reported = text
        .lines()
        .find_map(|line| line.strip_prefix(VERIFIED_PREFIX))
        .and_then(|pages| pages.trim().parse::<u64>().ok());
    if reported != Some(child_pages) {
        return Err(ProcbenchError::ChildReport {
            expected: format!("{VERIFIED_PREFIX}{child_pages}"),
            output: text.trim().to_owned(),
        });
    }
    let after = swap_counters()?;
    let pages_in = after.pages_in.saturating_sub(baseline.pages_in);
    if pages_in < child_pages {
        return Err(ProcbenchError::SwapInShort {
            pages_in,
            child_pages,
        });
    }

    println!("swap-cycle:{child_pages}");
    report_metric(
        "pages_out",
        after.pages_out.saturating_sub(baseline.pages_out),
    );
    report_metric("pages_in", pages_in);
    report_metric("hog_bytes", hog.chunks.len() * HOG_STEP_BYTES);
    Ok(())
}

/// The `swap-child` role: `swap-child <mib> <seed> <corrupt-page|->`.
pub(crate) async fn swap_child(args: impl Iterator<Item = String>) -> Result<(), ProcbenchError> {
    let mut args = args;
    let mib = parse_u64(args.next())?;
    let seed = parse_u64(args.next())?;
    let corrupt_page = match args.next().ok_or(ProcbenchError::Usage)?.as_str() {
        "-" => None,
        raw => Some(parse_u64(Some(raw.to_owned()))?),
    };
    let pages = mib * PAGES_PER_MIB;
    let bytes = usize::try_from(pages * PAGE_BYTES as u64).expect("region fits usize");

    let mut region = Vec::new();
    region
        .try_reserve_exact(bytes)
        .map_err(|_| ProcbenchError::HogAllocation { bytes })?;
    region.resize(bytes, 0);
    for (index, page) in region
        .as_chunks_mut::<PAGE_BYTES>()
        .0
        .iter_mut()
        .enumerate()
    {
        fill_page(seed, index as u64, page);
    }
    if let Some(page) = corrupt_page {
        // `Options::parse` bounds the index by the page count.
        region[usize::try_from(page).expect("page fits usize") * PAGE_BYTES] ^= 0xFF;
    }

    // Blocking here is what leaves the child idle: nothing runs in it
    // until the parent writes to its stdin.
    println!("{FILLED_PREFIX}{pages}");
    io::stdout()
        .flush()
        .map_err(|source| ProcbenchError::ChildIo { source })?;
    let mut wake = [0u8; 1];
    let read = io::stdin()
        .lock()
        .read(&mut wake)
        .map_err(|source| ProcbenchError::ChildIo { source })?;
    if read == 0 {
        return Err(ProcbenchError::NeverWoken);
    }

    let mut expected = [0u8; PAGE_BYTES];
    for (index, page) in region.as_chunks::<PAGE_BYTES>().0.iter().enumerate() {
        fill_page(seed, index as u64, &mut expected);
        if let Some(offset) = page.iter().zip(&expected).position(|(a, e)| a != e) {
            return Err(ProcbenchError::PageMismatch {
                page: index as u64,
                offset,
                expected: expected[offset],
                actual: page[offset],
            });
        }
    }
    println!("{VERIFIED_PREFIX}{pages}");
    io::stdout()
        .flush()
        .map_err(|source| ProcbenchError::ChildIo { source })
}

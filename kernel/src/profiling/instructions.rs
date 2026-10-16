//! Instruction-address sampling.
//!
//! The scoped profiler (`exec::observer`) names the phases the kernel
//! chose to time; it cannot say what ran inside one. This one records
//! the instruction a processor was executing when its periodic timer
//! interrupted it, so a phase can be split down to the function that
//! spent the time — a wasm function of a loaded module, a kernel
//! function, or neither.
//!
//! # Concurrency contract
//!
//! A backend that captures samples calls [`InstructionSampler::record`]
//! from its timer interrupt, on the processor that was interrupted. That
//! path takes no lock and allocates nothing: each processor owns one
//! fixed-capacity ring and is its only producer, and a full ring counts
//! the sample as dropped instead of waiting. The rings are allocated once,
//! the first time sampling starts, so a kernel that never profiles pays
//! for none of them.
//!
//! Everything else runs outside interrupt context, from any processor:
//! [`InstructionSampler::drain`] empties every ring under the sampler's
//! drain lock, which makes the lock holder each ring's only consumer, and
//! attributes each address while the code it names is still loaded. The
//! interrupt path never touches that lock, so it does not mask interrupts.
//!
//! Attribution is the caller's: the kernel knows its own image and the
//! runtime adapter knows the wasm code it loaded, so the drain is generic
//! over a [`CodeAttribution`] the adapter supplies.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;
use spin::{Mutex, Once};

use crate::Notify;

/// Samples one processor's ring holds before it drops.
///
/// The drain runs every [`SAMPLE_DRAIN_INTERVAL_MILLIS`]; at the 2 kHz the
/// x86 backend samples at, this is two seconds of samples a drain has not
/// reached, so a drain delayed by a busy executor loses nothing.
pub const SAMPLE_RING_CAPACITY: usize = 4096;

/// How often a running profile is drained and attributed, in milliseconds.
///
/// Attribution has to happen while the code a sample names is loaded; a
/// program that exits between its last sample and the drain leaves that
/// sample unattributed, so the interval is short against a program's life.
pub const SAMPLE_DRAIN_INTERVAL_MILLIS: u64 = 20;

/// What a backend that captures samples states about its capture.
///
/// A backend installs one with [`InstructionSampler::install_capture`];
/// a backend that installs none does not sample, and the profile export
/// says so rather than answering with an empty profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleCapture {
    /// The interval between two samples on one processor while sampling
    /// runs, in nanoseconds.
    pub period_nanos: u64,
    /// The runtime address the kernel image's lowest loadable segment was
    /// placed at. The host shifts a native address by it before looking
    /// the address up in the image's symbol table.
    pub kernel_image_base: u64,
}

/// What one sampled address was executing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SampledCode {
    /// A function of a loaded wasm module.
    Wasm(WasmFunctionSample),
    /// An address inside a loaded wasm image that belongs to none of its
    /// functions: the runtime's trampolines between the host and the guest.
    WasmTrampoline {
        /// The image the address lies in.
        module: Arc<str>,
    },
    /// Any other address, as it was sampled. The kernel image is the only
    /// other code that runs; the host decides whether the address lies in
    /// it, because the host holds the image's symbols.
    Native(u64),
}

/// A wasm function a sample landed in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WasmFunctionSample {
    /// The program the code was loaded for.
    pub module: Arc<str>,
    /// The core module within the program's component that defines the
    /// function; zero for a program that is one core module.
    pub static_module: u32,
    /// The function's index in its core module's function index space.
    pub index: u32,
    /// The function's name from the module's name section, when it has one.
    pub name: Option<Arc<str>>,
}

/// Names the code at a sampled address.
///
/// Implemented by whoever knows the code that can run: the runtime
/// adapter, which loaded every wasm image.
pub trait CodeAttribution {
    fn attribute(&self, address: usize) -> SampledCode;
}

/// Why the kernel cannot produce an instruction profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InstructionProfileError {
    /// The running backend does not capture instruction addresses.
    #[error("this backend does not sample instruction addresses")]
    NotCaptured,
}

/// Every sample taken since sampling last started, attributed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionProfile {
    pub capture: SampleCapture,
    /// Each distinct location and how many samples landed in it, the
    /// heaviest first.
    pub samples: Vec<(SampledCode, u64)>,
    /// Samples a full ring could not hold.
    pub dropped: u64,
}

/// The kernel's instruction-address sampler; see the module docs for its
/// concurrency contract.
#[derive(Clone)]
pub struct InstructionSampler {
    inner: Arc<SamplerInner>,
}

struct SamplerInner {
    processor_count: usize,
    capture: Once<SampleCapture>,
    /// Whether the backend should be sampling. Written by the profiling
    /// control, read by every processor's timer interrupt.
    sampling: AtomicBool,
    /// Raised each time sampling starts, for the drain that waits for it.
    started: Notify,
    /// One ring per processor, indexed by processor id, allocated the
    /// first time sampling starts.
    rings: Once<Box<[SampleRing]>>,
    /// The drain lock and what it has attributed so far.
    attributed: Mutex<Attributed>,
}

#[derive(Default)]
struct Attributed {
    counts: BTreeMap<SampledCode, u64>,
    dropped: u64,
}

/// One processor's single-producer, single-consumer ring of addresses.
struct SampleRing {
    /// Written by the owning processor only, from its timer interrupt.
    producer: CachePadded<RingProducer>,
    /// Written by the holder of the sampler's drain lock only.
    consumed: CachePadded<AtomicUsize>,
    slots: Box<[AtomicUsize]>,
}

struct RingProducer {
    /// Samples ever pushed; the slot of the next one is this modulo the
    /// capacity.
    pushed: AtomicUsize,
    /// Samples refused since the drain last collected the count.
    dropped: AtomicU64,
}

impl SampleRing {
    fn new() -> Self {
        Self {
            producer: CachePadded::new(RingProducer {
                pushed: AtomicUsize::new(0),
                dropped: AtomicU64::new(0),
            }),
            consumed: CachePadded::new(AtomicUsize::new(0)),
            slots: (0..SAMPLE_RING_CAPACITY)
                .map(|_| AtomicUsize::new(0))
                .collect(),
        }
    }

    /// Producer side: the owning processor's interrupt.
    fn push(&self, address: usize) {
        let pushed = self.producer.pushed.load(Ordering::Relaxed);
        // Acquire pairs with the consumer's release of the slots it has
        // read, so a slot is never overwritten before it was read.
        let consumed = self.consumed.load(Ordering::Acquire);
        if pushed.wrapping_sub(consumed) == SAMPLE_RING_CAPACITY {
            self.producer.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.slots[pushed % SAMPLE_RING_CAPACITY].store(address, Ordering::Relaxed);
        self.producer
            .pushed
            .store(pushed.wrapping_add(1), Ordering::Release);
    }

    /// Consumer side: the drain-lock holder. Hands every pending address
    /// to `sink` and returns the drop count collected with them.
    fn take(&self, mut sink: impl FnMut(usize)) -> u64 {
        let consumed = self.consumed.load(Ordering::Relaxed);
        let pushed = self.producer.pushed.load(Ordering::Acquire);
        let mut next = consumed;
        while next != pushed {
            sink(self.slots[next % SAMPLE_RING_CAPACITY].load(Ordering::Relaxed));
            next = next.wrapping_add(1);
        }
        self.consumed.store(pushed, Ordering::Release);
        self.producer.dropped.swap(0, Ordering::Relaxed)
    }
}

impl InstructionSampler {
    pub fn new(processor_count: usize) -> Self {
        assert!(
            processor_count != 0,
            "the instruction sampler needs at least one processor"
        );
        Self {
            inner: Arc::new(SamplerInner {
                processor_count,
                capture: Once::new(),
                sampling: AtomicBool::new(false),
                started: Notify::new(),
                rings: Once::new(),
                attributed: Mutex::new(Attributed::default()),
            }),
        }
    }

    /// Declares that the backend captures samples, and how. Called once,
    /// during bring-up, by a backend whose timer interrupt calls
    /// [`Self::record`].
    pub fn install_capture(&self, capture: SampleCapture) {
        assert!(
            self.inner.capture.get().is_none(),
            "instruction sample capture was installed more than once"
        );
        assert!(
            capture.period_nanos != 0,
            "an instruction sample capture needs a non-zero period"
        );
        self.inner.capture.call_once(|| capture);
    }

    /// Starts or stops sampling.
    ///
    /// Sampling is its own control, separate from the scoped profiler's:
    /// it shortens every processor's timer period, so it runs only when a
    /// reader asked for an instruction profile. Starting discards every
    /// sample an earlier run left, so a profile covers exactly one run,
    /// from its start to the read that follows. A backend that installed
    /// no capture refuses both directions.
    pub fn set_sampling(&self, enabled: bool) -> Result<(), InstructionProfileError> {
        if self.inner.capture.get().is_none() {
            return Err(InstructionProfileError::NotCaptured);
        }
        if !enabled {
            self.inner.sampling.store(false, Ordering::Release);
            return Ok(());
        }
        let processor_count = self.inner.processor_count;
        self.inner.rings.call_once(|| {
            (0..processor_count)
                .map(|_| SampleRing::new())
                .collect::<Box<[_]>>()
        });
        self.discard();
        // Release publishes the rings to every processor whose interrupt
        // reads the flag with acquire.
        self.inner.sampling.store(true, Ordering::Release);
        self.inner.started.notify_all();
        Ok(())
    }

    /// Whether the backend should be sampling.
    pub fn sampling(&self) -> bool {
        self.inner.sampling.load(Ordering::Acquire)
    }

    /// Resolves once sampling is running.
    pub async fn sampling_started(&self) {
        loop {
            // Armed before the flag is tested: a start between the test
            // and the park completes this wait.
            let started = self.inner.started.notified();
            if self.sampling() {
                return;
            }
            started.await;
        }
    }

    /// Records the address the executing processor was interrupted at.
    ///
    /// Interrupt context: no lock, no allocation. A backend calls it only
    /// once [`Self::sampling`] has answered `true` on this processor, and
    /// may keep calling it until it observes `false`.
    pub fn record(&self, address: usize) {
        let rings =
            self.inner.rings.get().unwrap_or_else(|| {
                panic!("an instruction sample was recorded before sampling ran")
            });
        rings[usize::from(current_processor().id())].push(address);
    }

    /// Empties every ring and attributes what it held.
    pub fn drain<Attribution: CodeAttribution>(&self, attribution: &Attribution) {
        let Some(rings) = self.inner.rings.get() else {
            return;
        };
        let mut attributed = self.inner.attributed.lock();
        let Attributed { counts, dropped } = &mut *attributed;
        for ring in rings.iter() {
            let ring_dropped = ring.take(|address| {
                *counts.entry(attribution.attribute(address)).or_insert(0) += 1;
            });
            *dropped = dropped.saturating_add(ring_dropped);
        }
    }

    /// Discards every sample taken so far, attributed or not.
    fn discard(&self) {
        let mut attributed = self.inner.attributed.lock();
        if let Some(rings) = self.inner.rings.get() {
            for ring in rings.iter() {
                ring.take(|_| {});
            }
        }
        *attributed = Attributed::default();
    }

    /// Drains, then reports every sample since sampling last started.
    pub fn profile<Attribution: CodeAttribution>(
        &self,
        attribution: &Attribution,
    ) -> Result<InstructionProfile, InstructionProfileError> {
        let capture = *self
            .inner
            .capture
            .get()
            .ok_or(InstructionProfileError::NotCaptured)?;
        self.drain(attribution);
        let attributed = self.inner.attributed.lock();
        let mut samples = attributed
            .counts
            .iter()
            .map(|(code, count)| (code.clone(), *count))
            .collect::<Vec<_>>();
        samples.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        Ok(InstructionProfile {
            capture,
            samples,
            dropped: attributed.dropped,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::{
        CodeAttribution, InstructionProfileError, InstructionSampler, SAMPLE_RING_CAPACITY,
        SampleCapture, SampledCode, WasmFunctionSample,
    };

    const CAPTURE: SampleCapture = SampleCapture {
        period_nanos: 1_000_000,
        kernel_image_base: 0xffff_ffff_8000_0000,
    };

    /// Addresses at or above 0x1000 are one wasm function; the rest are
    /// native.
    struct Split;

    impl CodeAttribution for Split {
        fn attribute(&self, address: usize) -> SampledCode {
            if address >= 0x1000 {
                SampledCode::Wasm(WasmFunctionSample {
                    module: Arc::from("loop"),
                    static_module: 0,
                    index: 3,
                    name: Some(Arc::from("spin")),
                })
            } else {
                SampledCode::Native(address as u64)
            }
        }
    }

    #[test]
    fn a_backend_without_capture_reports_it_rather_than_an_empty_profile() {
        let sampler = InstructionSampler::new(1);
        assert_eq!(
            sampler.set_sampling(true),
            Err(InstructionProfileError::NotCaptured)
        );
        assert!(!sampler.sampling());
        assert_eq!(
            sampler.profile(&Split),
            Err(InstructionProfileError::NotCaptured)
        );
    }

    #[test]
    fn samples_are_attributed_and_ranked() {
        let sampler = InstructionSampler::new(1);
        sampler.install_capture(CAPTURE);
        sampler.set_sampling(true).expect("capture is installed");
        assert!(sampler.sampling());
        for address in [0x1000, 0x1004, 0x10, 0x1008] {
            sampler.record(address);
        }
        sampler.drain(&Split);
        sampler.record(0x10);

        let profile = sampler.profile(&Split).expect("capture is installed");
        assert_eq!(profile.capture, CAPTURE);
        assert_eq!(profile.dropped, 0);
        assert_eq!(profile.samples.len(), 2);
        assert!(
            matches!(&profile.samples[0].0, SampledCode::Wasm(function) if function.index == 3)
        );
        assert_eq!(profile.samples[0].1, 3);
        assert_eq!(profile.samples[1], (SampledCode::Native(0x10), 2));
    }

    #[test]
    fn a_full_ring_counts_drops_and_keeps_what_it_holds() {
        let sampler = InstructionSampler::new(1);
        sampler.install_capture(CAPTURE);
        sampler.set_sampling(true).expect("capture is installed");
        for _ in 0..SAMPLE_RING_CAPACITY + 5 {
            sampler.record(0x20);
        }
        let profile = sampler.profile(&Split).expect("capture is installed");
        assert_eq!(profile.dropped, 5);
        assert_eq!(
            profile.samples,
            [(SampledCode::Native(0x20), SAMPLE_RING_CAPACITY as u64)]
        );

        // The ring wraps and keeps accepting once drained.
        sampler.record(0x30);
        let profile = sampler.profile(&Split).expect("capture is installed");
        assert_eq!(profile.dropped, 5);
        assert_eq!(profile.samples.len(), 2);
    }

    #[test]
    fn starting_a_run_discards_the_previous_runs_samples() {
        let sampler = InstructionSampler::new(1);
        sampler.install_capture(CAPTURE);
        sampler.set_sampling(true).expect("capture is installed");
        sampler.record(0x10);
        sampler.drain(&Split);
        sampler.record(0x1000);
        sampler.set_sampling(false).expect("capture is installed");
        sampler.set_sampling(true).expect("capture is installed");
        let profile = sampler.profile(&Split).expect("capture is installed");
        assert!(profile.samples.is_empty());
        assert_eq!(profile.dropped, 0);
    }
}

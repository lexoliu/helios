//! Kernel entropy: one root DRBG per boot, and the per-instance pools
//! derived from it.
//!
//! Every cryptographic byte the kernel or a guest sees comes from
//! [`RootEntropy`], a ChaCha20 DRBG seeded at boot from every source the
//! platform actually has. There are three, and a platform needs one:
//! the processor's own instruction or host source (`RDSEED`/`RDRAND`,
//! `RNDR`, the host OS), the seed firmware handed us in the device
//! tree's `/chosen/rng-seed`, and a read of the platform's entropy
//! device taken during bring-up. A kernel that finds none of them fails
//! to boot rather than running with a predictable stream.
//!
//! The third exists because the first two are not always there. An
//! AArch64 machine described by ACPI has no `/chosen/rng-seed` — ACPI
//! has no counterpart to it — and a processor without `FEAT_RNG` has no
//! instruction either, which leaves the entropy device as the only
//! source such a machine has. The same device goes on reseeding the
//! root from a kernel task once the executor runs; the bring-up read is
//! what makes the boot possible at all.
//!
//! Concurrency contract: the root DRBG is a spin mutex held for the
//! length of one generate or reseed and never across an await. Reads of
//! the device are the only part that awaits, and they happen in a kernel
//! task outside the lock. Every draw first observes the machine
//! generation (lock-free while it is unchanged, see
//! [`MachineGeneration`]) and rekeys before it produces a byte when the
//! machine was restored. Per-instance [`EntropyPool`]s are owned by
//! their instance and take no lock at all.

extern crate alloc;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use futures::future::Either;
use helios_hal::cpu::Cpu;
use helios_hal::entropy::EntropyQuality;
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use sha2::{Digest, Sha256};
use spin::Mutex;
use thiserror::Error;
use triomphe::Arc;

use crate::machine::{Generation, MachineGeneration, MachineGenerationHandle};
use crate::{Kernel, Timer};

/// Bytes the root DRBG draws from a hardware source per seed or reseed.
pub const ROOT_ENTROPY_MATERIAL_BYTES: usize = 64;

/// Domain separators, so material that reaches the hash through two
/// different paths cannot collide into the same key.
const SEED_DOMAIN: &[u8] = b"helios.entropy.root.seed.v1";
const RESEED_DOMAIN: &[u8] = b"helios.entropy.root.reseed.v1";
const DERIVE_DOMAIN: &[u8] = b"helios.entropy.pool.derive.v1";
const REKEY_DOMAIN: &[u8] = b"helios.entropy.root.rekey.v1";

/// A kernel image that reaches this has no way to produce unpredictable
/// bytes, so it cannot serve a guest's random source, seed an address-space
/// layout, or pick a TCP initial sequence number. Booting on regardless
/// would make every one of those predictable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error(
    "the platform exposes no cryptographic entropy source: the processor has no entropy \
     instruction, firmware left no seed, and no entropy device answered at bring-up"
)]
pub struct NoCryptographicEntropy;

/// A shared handle to the boot-seeded root DRBG.
///
/// The runtime state and the reseed task both hold one for the life of
/// the boot, which is the ownership the handle exists to express.
pub type RootEntropyHandle = Arc<RootEntropy>;

/// Which sources contributed to the root DRBG's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntropySources {
    /// `Cpu::fill_entropy` reported [`EntropyQuality::Cryptographic`].
    pub cpu: bool,
    /// Firmware handed the kernel a seed, such as the device tree's
    /// `/chosen/rng-seed`.
    pub firmware: bool,
    /// The platform's entropy device has contributed, either through
    /// the bring-up read that seeded the root or through a later
    /// reseed.
    pub device: bool,
}

impl EntropySources {
    /// Whether any source has contributed unpredictable bytes.
    pub const fn any(&self) -> bool {
        self.cpu || self.firmware || self.device
    }
}

/// The kernel's root DRBG.
pub struct RootEntropy {
    state: Mutex<RootState>,
    generation: MachineGenerationHandle,
}

struct RootState {
    drbg: ChaCha20Rng,
    sources: EntropySources,
    reseeds: u64,
    generated: u64,
    /// The machine generation the stream was last keyed for.
    generation: Generation,
    rekeys: u64,
}

impl RootEntropy {
    /// Seeds the root DRBG from every source the platform has before
    /// the executor runs: the processor's own, whatever seed firmware
    /// left behind, and a bring-up read of the entropy device.
    ///
    /// None is a fallback for another — every source present is mixed
    /// in, and the result is unpredictable as long as one of them was.
    /// The device goes on contributing after boot through
    /// [`Self::reseed`].
    pub fn from_platform<CpuImpl>(
        cpu: &CpuImpl,
        firmware_seed: Option<&[u8]>,
        device_material: Option<&[u8; ROOT_ENTROPY_MATERIAL_BYTES]>,
        generation: MachineGenerationHandle,
    ) -> Result<Self, NoCryptographicEntropy>
    where
        CpuImpl: Cpu,
    {
        let mut hash = Sha256::new();
        hash.update(SEED_DOMAIN);
        let mut sources = EntropySources::default();

        let mut cpu_material = [0_u8; ROOT_ENTROPY_MATERIAL_BYTES];
        if cpu.fill_entropy(&mut cpu_material) == Ok(EntropyQuality::Cryptographic) {
            hash.update(cpu_material);
            sources.cpu = true;
        }
        if let Some(seed) = firmware_seed {
            assert!(
                !seed.is_empty(),
                "firmware offered an empty entropy seed; omit the source instead"
            );
            hash.update(seed);
            sources.firmware = true;
        }
        if let Some(material) = device_material {
            hash.update(material);
            sources.device = true;
        }

        if !sources.any() {
            return Err(NoCryptographicEntropy);
        }

        // The stream is keyed for the generation the machine is in now.
        let keyed_for = generation.observe();
        Ok(Self {
            state: Mutex::new(RootState {
                drbg: ChaCha20Rng::from_seed(hash.finalize().into()),
                sources,
                reseeds: 0,
                generated: 0,
                generation: keyed_for,
                rekeys: 0,
            }),
            generation,
        })
    }

    /// The machine generation this root keeps its stream unique to.
    pub fn generation(&self) -> &MachineGeneration {
        &self.generation
    }

    /// Mixes fresh hardware material into the root DRBG, and records the
    /// platform's entropy device as a source.
    ///
    /// The current state is folded in alongside the new material, so a
    /// device that turns out to produce poor bytes can only ever add to
    /// what the root already had.
    pub fn reseed(&self, material: &[u8; ROOT_ENTROPY_MATERIAL_BYTES]) {
        let mut state = self.state.lock();
        Self::fold(&mut state, material);
        state.sources.device = true;
        state.reseeds = state.reseeds.saturating_add(1);
    }

    /// Mixes fresh processor-sourced material into the root DRBG, as
    /// [`Self::reseed`] does for the device's.
    pub fn reseed_from_cpu(&self, material: &[u8; ROOT_ENTROPY_MATERIAL_BYTES]) {
        let mut state = self.state.lock();
        Self::fold(&mut state, material);
        state.sources.cpu = true;
    }

    fn fold(state: &mut RootState, material: &[u8; ROOT_ENTROPY_MATERIAL_BYTES]) {
        let mut carry = [0_u8; 32];
        state.drbg.fill_bytes(&mut carry);

        let mut hash = Sha256::new();
        hash.update(RESEED_DOMAIN);
        hash.update(carry);
        hash.update(material);
        state.drbg = ChaCha20Rng::from_seed(hash.finalize().into());
        state.generated = 0;
    }

    /// Fills `buffer` from the root DRBG.
    ///
    /// The machine generation is observed first, and when it moved the
    /// stream is rekeyed before a byte is produced, so no byte leaves
    /// the root that a clone of this machine also holds. The new VM
    /// generation ID is not secret from the host that wrote it: it
    /// guarantees the copies diverge, and unpredictability against
    /// someone who holds the snapshot returns with the processor and
    /// device material the reseed task folds in right after.
    pub fn fill(&self, buffer: &mut [u8]) {
        let observed = self.generation.observe();
        let mut state = self.state.lock();
        if state.generation != observed {
            // Which advance the root folds in is whatever is latest when
            // the lock is held, which is never older than `observed`.
            let (latest, cause) = self.generation.latest_advance();
            Self::rekey(&mut state, latest, &cause);
        }
        state.drbg.fill_bytes(buffer);
        state.generated = state.generated.saturating_add(buffer.len() as u64);
    }

    /// Folds the cause of a generation advance into the stream, the way
    /// a reseed folds material in: the old state is carried forward, so
    /// the new key depends on everything the root held and on the cause.
    fn rekey(state: &mut RootState, generation: Generation, cause: &[u8; 16]) {
        let mut carry = [0_u8; 32];
        state.drbg.fill_bytes(&mut carry);

        let mut hash = Sha256::new();
        hash.update(REKEY_DOMAIN);
        hash.update(carry);
        hash.update(cause);
        hash.update(generation.get().to_le_bytes());
        state.drbg = ChaCha20Rng::from_seed(hash.finalize().into());
        state.generation = generation;
        state.rekeys = state.rekeys.saturating_add(1);
        state.generated = 0;
    }

    /// How many times a machine-generation advance has rekeyed the root.
    pub fn rekey_count(&self) -> u64 {
        self.state.lock().rekeys
    }

    /// The sources that have contributed to the root's state so far.
    pub fn sources(&self) -> EntropySources {
        self.state.lock().sources
    }

    /// How many times a hardware source has reseeded the root.
    pub fn reseed_count(&self) -> u64 {
        self.state.lock().reseeds
    }

    /// Bytes generated since the last reseed.
    pub fn generated_since_reseed(&self) -> u64 {
        self.state.lock().generated
    }
}

/// Seeds the root DRBG for this boot, or fails the boot.
///
/// Backends call this once on the bootstrap processor with the seed
/// firmware left them — the device tree's `/chosen/rng-seed` on the
/// platforms that have one — and the entropy device they have already
/// brought up, and install the result on the runtime state. A platform
/// with no cryptographic source at all panics here: the alternative is
/// a kernel whose guest randomness, address-space layout and TCP
/// sequence numbers are all predictable, which is worse than not
/// booting.
///
/// The device is read here rather than in each backend so the three of
/// them share one implementation, and it must already be programmed:
/// this is the same device the caller later hands to
/// [`install_entropy_device`] for continuous reseeding.
///
/// `generation` is the machine generation the backend created for this
/// boot: the root rekeys itself whenever it advances.
pub fn seed_root_entropy<CpuImpl, Device>(
    cpu: &CpuImpl,
    firmware_seed: Option<&[u8]>,
    device: Option<&Device>,
    generation: MachineGenerationHandle,
) -> RootEntropyHandle
where
    CpuImpl: Cpu,
    Device: HardwareEntropySource,
{
    let device_material = device.and_then(|device| match read_at_bring_up(device) {
        Ok(material) => Some(material),
        Err(error) => {
            // Not fatal on its own: a platform that also has a
            // processor source or a firmware seed is still seeded from
            // those, and one that has neither fails immediately below.
            tracing::warn!("the platform entropy device did not answer at bring-up: {error:?}");
            None
        }
    });
    let root = RootEntropy::from_platform(cpu, firmware_seed, device_material.as_ref(), generation)
        .unwrap_or_else(|error| panic!("{error}"));
    let sources = root.sources();
    tracing::info!(
        cpu = sources.cpu,
        firmware = sources.firmware,
        device = sources.device,
        "root entropy seeded"
    );
    Arc::new(root)
}

/// Reads seed material from an entropy device before the executor
/// exists.
///
/// This is one of the `block_on` sites AGENTS.md §4 allows: a bootstrap
/// entry point that runs before the kernel executor starts, on the
/// bootstrap processor, with nothing else to yield to. Polling rather
/// than awaiting is sound for this device because the virtio completion
/// path drains its own used ring on every poll — the device interrupt
/// exists to spare the reader from spinning, not to deliver the
/// completion — so the spin waits on the descriptor the device is
/// writing and not on software state.
fn read_at_bring_up<Device>(device: &Device) -> Result<[u8; ROOT_ENTROPY_MATERIAL_BYTES], IoError>
where
    Device: HardwareEntropySource,
{
    let mut material = [0_u8; ROOT_ENTROPY_MATERIAL_BYTES];
    {
        let mut fill = pin!(device.fill(&mut material));
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match fill.as_mut().poll(&mut context) {
                Poll::Ready(result) => break result?,
                Poll::Pending => {
                    // Nothing else runs on this processor yet, so the
                    // completion has to be collected by this loop
                    // rather than by the handler that will own the
                    // device once interrupts are unmasked.
                    device.drain_completions();
                    core::hint::spin_loop();
                }
            }
        }
    }
    Ok(material)
}

/// A platform with no entropy device the kernel can read at bring-up.
///
/// Uninhabited, so a backend that names it as the device type is
/// stating a fact the type system enforces rather than supplying a
/// stub that would have to answer somehow. The hosted backend is the
/// one that uses it: its only cryptographic source is the host OS,
/// which reaches the kernel through `Cpu::fill_entropy`.
pub enum NoEntropyDevice {}

impl HardwareEntropySource for NoEntropyDevice {
    #[expect(
        unreachable_code,
        reason = "`Self` is uninhabited, so the empty match diverges; the call it feeds exists \
                  only to give the signature a concrete future type"
    )]
    fn fill<'a>(
        &'a self,
        _buffer: &'a mut [u8],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        core::future::ready(match *self {})
    }

    fn drain_completions(&self) {
        match *self {}
    }
}

/// A hardware entropy device the kernel can read.
///
/// Backends implement this over whatever their platform exposes — today
/// that is always a virtio-entropy device — and the kernel treats it as
/// a continuous source it folds into [`RootEntropy`]. Reads are async
/// because a real device answers through an interrupt.
pub trait HardwareEntropySource: Send + 'static {
    fn fill<'a>(
        &'a self,
        buffer: &'a mut [u8],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;

    /// Collects whatever the device has finished, without waiting for
    /// its interrupt.
    ///
    /// This is the same work the device's interrupt handler does, and
    /// it exists as its own method for the one caller that has no
    /// interrupt to rely on: the bring-up read in
    /// [`seed_root_entropy`], which runs before the executor exists and
    /// before interrupts are unmasked. A completion the driver would
    /// otherwise learn about from its handler is noticed here instead.
    fn drain_completions(&self);
}

/// How long the reseed task waits between hardware reads.
///
/// ChaCha20's stream is safe far beyond anything a kernel generates
/// between two ticks of this interval, so the interval exists to bound
/// how long a compromised state could persist, not to bound output
/// volume. A wall-clock interval is also the only trigger that does not
/// need a wake-up channel on the generate path, which runs under the
/// root's lock.
pub const ENTROPY_RESEED_INTERVAL: Duration = Duration::from_secs(60);

/// Spawns the task that keeps the root DRBG reseeded from `device`.
///
/// The task's first read is the device's contribution to the boot seed:
/// it runs as soon as the executor starts, which is before any component
/// can ask for random bytes. Reads never block the executor — the driver
/// parks on the device's interrupt — and a failed read leaves the root
/// with the state it already had.
///
/// The task is local to the calling processor because the device's
/// completions are: a backend routes the entropy interrupt to the
/// processor that brought the device up, so its sole reader belongs on
/// that processor's run queue rather than on the work-stealing one.
pub fn install_entropy_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    root: RootEntropyHandle,
    device: Device,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: HardwareEntropySource,
{
    let timer = kernel.timer();
    kernel.spawn_local_detached(async move {
        reseed_forever(root, device, timer).await;
    });
}

async fn reseed_forever<CpuImpl, Device>(
    root: RootEntropyHandle,
    device: Device,
    timer: Timer<CpuImpl>,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    Device: HardwareEntropySource,
{
    tracing::info!(
        interval_secs = ENTROPY_RESEED_INTERVAL.as_secs(),
        "root entropy reseed task started"
    );
    let mut material = [0_u8; ROOT_ENTROPY_MATERIAL_BYTES];
    loop {
        reseed_from_device(&root, &device, &mut material).await;
        // The wait is armed by creating it, and a permit raised while
        // the device was read is still there to be claimed, so an
        // advance during the read is not lost.
        let advanced = pin!(root.generation().entropy_wake().notified());
        let interval = pin!(timer.sleep_for(ENTROPY_RESEED_INTERVAL));
        let advanced_first = matches!(
            futures::future::select(interval, advanced).await,
            Either::Right(_)
        );
        if advanced_first {
            // The machine was restored: the ID alone makes the copies
            // diverge, and the processor's own source is what makes the
            // stream unpredictable to someone holding the snapshot. The
            // device read follows at the top of the loop.
            reseed_from_cpu(&root, timer.cpu(), &mut material);
            tracing::info!(
                generation = root.generation().current().get(),
                "machine generation advanced; root entropy takes fresh material at once"
            );
        }
    }
}

fn reseed_from_cpu<CpuImpl: Cpu>(
    root: &RootEntropy,
    cpu: &CpuImpl,
    material: &mut [u8; ROOT_ENTROPY_MATERIAL_BYTES],
) {
    if cpu.fill_entropy(material) == Ok(EntropyQuality::Cryptographic) {
        root.reseed_from_cpu(material);
    }
    material.fill(0);
}

async fn reseed_from_device<Device>(
    root: &RootEntropy,
    device: &Device,
    material: &mut [u8; ROOT_ENTROPY_MATERIAL_BYTES],
) where
    Device: HardwareEntropySource,
{
    match device.fill(material).await {
        Ok(()) => {
            root.reseed(material);
            // The material has been folded into the root; leaving a
            // copy on the task's stack across the sleep would keep
            // raw device output live for a whole interval.
            material.fill(0);
            let reseeds = root.reseed_count();
            if reseeds == 1 {
                tracing::info!(
                    bytes = ROOT_ENTROPY_MATERIAL_BYTES,
                    "root entropy seeded from the platform entropy device"
                );
            } else {
                tracing::debug!(reseeds, "root entropy reseeded");
            }
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                "platform entropy device read failed; the root DRBG keeps its current state"
            );
        }
    }
}

/// One instance's random streams.
///
/// A pool is derived from the root DRBG when its instance is created and
/// is owned by that instance afterwards, so drawing from it costs no
/// lock and cannot be observed by another instance.
///
/// Every draw takes the root the pool was derived from. The pool holds
/// the machine generation it was derived under, and a draw first
/// compares it with the root's: when the machine was restored in
/// between, the pool derives itself afresh from the (rekeyed) root
/// before it produces a byte, so a clone of the machine never replays
/// the instance's stream. Requiring the root in the signature is what
/// makes that check impossible to skip.
pub struct EntropyPool {
    secure: ChaCha20Rng,
    insecure: ChaCha20Rng,
    insecure_seed: ChaCha20Rng,
    personalization: u64,
    generation: Generation,
}

impl EntropyPool {
    /// Derives an instance pool from the root DRBG.
    ///
    /// `personalization` separates two instances that draw at the same
    /// moment; the root's own output is what makes the streams
    /// unpredictable.
    pub fn derive(root: &RootEntropy, personalization: u64) -> Self {
        // Observed before the root is drawn from: an advance after this
        // line leaves the pool one generation behind, and its first draw
        // derives it again, which costs a derivation and loses nothing.
        let generation = root.generation().observe();
        let mut root_material = [0_u8; 96];
        root.fill(&mut root_material);

        let mut seeds = [[0_u8; 32]; 3];
        for (index, seed) in seeds.iter_mut().enumerate() {
            let mut hash = Sha256::new();
            hash.update(DERIVE_DOMAIN);
            hash.update(personalization.to_le_bytes());
            hash.update((index as u64).to_le_bytes());
            hash.update(root_material);
            *seed = hash.finalize().into();
        }

        Self {
            secure: ChaCha20Rng::from_seed(seeds[0]),
            insecure: ChaCha20Rng::from_seed(seeds[1]),
            insecure_seed: ChaCha20Rng::from_seed(seeds[2]),
            personalization,
            generation,
        }
    }

    /// Derives the pool again when the machine generation moved since
    /// the pool was derived. Every draw calls this first.
    fn rekey_if_restored(&mut self, root: &RootEntropy) {
        if root.generation().observe() != self.generation {
            *self = Self::derive(root, self.personalization);
        }
    }

    pub fn fill_secure(&mut self, root: &RootEntropy, buffer: &mut [u8]) {
        self.rekey_if_restored(root);
        self.secure.fill_bytes(buffer);
    }

    pub fn secure_u64(&mut self, root: &RootEntropy) -> u64 {
        self.rekey_if_restored(root);
        self.secure.next_u64()
    }

    pub fn fill_insecure(&mut self, root: &RootEntropy, buffer: &mut [u8]) {
        self.rekey_if_restored(root);
        self.insecure.fill_bytes(buffer);
    }

    pub fn insecure_u64(&mut self, root: &RootEntropy) -> u64 {
        self.rekey_if_restored(root);
        self.insecure.next_u64()
    }

    pub fn insecure_seed(&mut self, root: &RootEntropy) -> (u64, u64) {
        self.rekey_if_restored(root);
        (self.insecure_seed.next_u64(), self.insecure_seed.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use helios_hal::generation::VmGenerationId;

    use super::{
        EntropyPool, EntropySources, NoCryptographicEntropy, ROOT_ENTROPY_MATERIAL_BYTES,
        RootEntropy,
    };
    use crate::machine::test_window::TestWindow;
    use crate::machine::{AdvanceCause, MachineGeneration};
    use crate::test_support::TestCpu;

    /// A root on a machine whose VMM publishes no generation window.
    fn from_platform(
        cpu: &TestCpu,
        firmware_seed: Option<&[u8]>,
        device_material: Option<&[u8; ROOT_ENTROPY_MATERIAL_BYTES]>,
    ) -> Result<RootEntropy, NoCryptographicEntropy> {
        RootEntropy::from_platform(
            cpu,
            firmware_seed,
            device_material,
            MachineGeneration::new(None),
        )
    }

    fn root() -> RootEntropy {
        from_platform(&TestCpu::with_entropy(0x5a), None, None)
            .expect("a CPU source is enough to seed the root")
    }

    fn first_bytes(root: &RootEntropy) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        root.fill(&mut bytes);
        bytes
    }

    #[test]
    fn a_platform_without_any_source_cannot_seed_the_root() {
        assert_eq!(
            from_platform(&TestCpu::without_entropy(), None, None).err(),
            Some(NoCryptographicEntropy)
        );
    }

    #[test]
    fn firmware_alone_seeds_the_root() {
        let root = from_platform(&TestCpu::without_entropy(), Some(&[7_u8; 32]), None)
            .expect("a firmware seed is a cryptographic source");
        assert_eq!(
            root.sources(),
            EntropySources {
                cpu: false,
                firmware: true,
                device: false,
            }
        );
        assert_ne!(first_bytes(&root), [0_u8; 32]);
    }

    #[test]
    fn a_bring_up_device_read_alone_seeds_the_root() {
        // The case an ACPI-described AArch64 machine without FEAT_RNG
        // is in: no processor source, no firmware seed, and only the
        // entropy device read during bring-up.
        let root = from_platform(&TestCpu::without_entropy(), None, Some(&[9_u8; 64]))
            .expect("a bring-up device read is a cryptographic source");
        assert_eq!(
            root.sources(),
            EntropySources {
                cpu: false,
                firmware: false,
                device: true,
            }
        );
        assert_ne!(first_bytes(&root), [0_u8; 32]);
    }

    #[test]
    fn a_bring_up_device_read_is_mixed_in_rather_than_replacing_the_others() {
        let without = from_platform(&TestCpu::with_entropy(0x5a), None, None)
            .expect("cpu source seeds the root");
        let with = from_platform(
            &TestCpu::with_entropy(0x5a),
            None,
            Some(&[1_u8; ROOT_ENTROPY_MATERIAL_BYTES]),
        )
        .expect("cpu and device seed the root");
        assert_ne!(first_bytes(&without), first_bytes(&with));
        assert_eq!(
            with.sources(),
            EntropySources {
                cpu: true,
                firmware: false,
                device: true,
            }
        );
    }

    #[test]
    fn every_present_source_changes_the_seed() {
        // Firmware is mixed in, not used as a fallback for the CPU: two
        // roots that share a CPU source but differ in the firmware seed
        // must not produce the same stream.
        let cpu_only = from_platform(&TestCpu::with_entropy(0x5a), None, None)
            .expect("cpu source seeds the root");
        let with_firmware = from_platform(&TestCpu::with_entropy(0x5a), Some(&[3_u8; 32]), None)
            .expect("cpu and firmware seed the root");
        let other_firmware = from_platform(&TestCpu::with_entropy(0x5a), Some(&[4_u8; 32]), None)
            .expect("cpu and firmware seed the root");

        assert_eq!(
            with_firmware.sources(),
            EntropySources {
                cpu: true,
                firmware: true,
                device: false,
            }
        );
        assert_ne!(first_bytes(&cpu_only), first_bytes(&with_firmware));
        assert_ne!(first_bytes(&with_firmware), first_bytes(&other_firmware));
    }

    #[test]
    fn a_reseed_records_its_source_and_changes_the_stream() {
        let root = root();
        assert_eq!(root.reseed_count(), 0);
        assert!(!root.sources().device);

        let before = first_bytes(&root);
        assert_eq!(root.generated_since_reseed(), 32);

        root.reseed(&[0xa5_u8; ROOT_ENTROPY_MATERIAL_BYTES]);
        assert_eq!(root.reseed_count(), 1);
        assert!(root.sources().device);
        assert_eq!(
            root.generated_since_reseed(),
            0,
            "the output budget restarts at a reseed"
        );
        assert_ne!(before, first_bytes(&root));

        root.reseed(&[0xa5_u8; ROOT_ENTROPY_MATERIAL_BYTES]);
        assert_eq!(root.reseed_count(), 2);
    }

    #[test]
    fn a_reseed_carries_the_previous_state_forward() {
        // Identical device material on two roots that differ only in
        // their boot seed must still yield different streams: the
        // reseed folds the old state in rather than replacing it.
        let first = from_platform(&TestCpu::with_entropy(1), None, None).expect("seeded");
        let second = from_platform(&TestCpu::with_entropy(2), None, None).expect("seeded");

        first.reseed(&[9_u8; ROOT_ENTROPY_MATERIAL_BYTES]);
        second.reseed(&[9_u8; ROOT_ENTROPY_MATERIAL_BYTES]);

        assert_ne!(first_bytes(&first), first_bytes(&second));
    }

    #[test]
    fn derived_pools_differ_per_personalization() {
        let root = root();
        let mut first = EntropyPool::derive(&root, 1);
        let mut second = EntropyPool::derive(&root, 2);

        let mut first_bytes = [0_u8; 32];
        let mut second_bytes = [0_u8; 32];
        first.fill_secure(&root, &mut first_bytes);
        second.fill_secure(&root, &mut second_bytes);

        assert_ne!(first_bytes, [0_u8; 32]);
        assert_ne!(first_bytes, second_bytes);
    }

    #[test]
    fn a_pools_streams_are_independent_of_each_other() {
        let root = root();
        let mut pool = EntropyPool::derive(&root, 7);

        assert_ne!(pool.secure_u64(&root), pool.insecure_u64(&root));
        assert_ne!(pool.insecure_seed(&root), (0, 0));
        let mut insecure = [0_u8; 16];
        pool.fill_insecure(&root, &mut insecure);
        assert_ne!(insecure, [0_u8; 16]);
    }

    #[test]
    fn two_pools_derived_alike_still_differ_because_the_root_advances() {
        let root = root();
        let mut first = EntropyPool::derive(&root, 42);
        let mut second = EntropyPool::derive(&root, 42);

        assert_ne!(first.secure_u64(&root), second.secure_u64(&root));
    }

    fn id(byte: u8) -> VmGenerationId {
        VmGenerationId::from_bytes([byte; 16])
    }

    /// A root and one instance pool in a state a snapshot would hold,
    /// on a machine whose window shows `id(1)`. Calling it twice gives
    /// two machines restored from the same snapshot.
    fn restored_machine() -> (TestWindow, RootEntropy, EntropyPool) {
        let (window, handle) = TestWindow::new(id(1));
        let root = RootEntropy::from_platform(
            &TestCpu::with_entropy(0x5a),
            None,
            None,
            MachineGeneration::new(Some(handle)),
        )
        .expect("a CPU source is enough to seed the root");
        let pool = EntropyPool::derive(&root, 42);
        (window, root, pool)
    }

    fn pool_first_bytes(pool: &mut EntropyPool, root: &RootEntropy) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        pool.fill_secure(root, &mut bytes);
        bytes
    }

    #[test]
    fn clones_restored_under_different_generation_ids_diverge() {
        let (first_window, first_root, mut first_pool) = restored_machine();
        let (second_window, second_root, mut second_pool) = restored_machine();
        first_window.write(id(2));
        second_window.write(id(3));

        assert_ne!(first_bytes(&first_root), first_bytes(&second_root));
        assert_ne!(
            pool_first_bytes(&mut first_pool, &first_root),
            pool_first_bytes(&mut second_pool, &second_root)
        );
        assert_eq!(first_root.rekey_count(), 1);
    }

    #[test]
    fn clones_whose_generation_did_not_move_stay_identical() {
        let (_, first_root, mut first_pool) = restored_machine();
        let (_, second_root, mut second_pool) = restored_machine();

        assert_eq!(first_bytes(&first_root), first_bytes(&second_root));
        assert_eq!(
            pool_first_bytes(&mut first_pool, &first_root),
            pool_first_bytes(&mut second_pool, &second_root)
        );
        assert_eq!(first_root.rekey_count(), 0);
    }

    #[test]
    fn clones_restored_under_the_same_generation_id_agree() {
        let (first_window, first_root, mut first_pool) = restored_machine();
        let (second_window, second_root, mut second_pool) = restored_machine();
        first_window.write(id(2));
        second_window.write(id(2));

        assert_eq!(first_bytes(&first_root), first_bytes(&second_root));
        assert_eq!(
            pool_first_bytes(&mut first_pool, &first_root),
            pool_first_bytes(&mut second_pool, &second_root)
        );
    }

    #[test]
    fn a_pool_drawn_after_an_advance_differs_from_the_same_pool_without_one() {
        let (_, root_without, mut pool_without) = restored_machine();
        let (window, root_with, mut pool_with) = restored_machine();
        window.write(id(2));

        assert_ne!(
            pool_first_bytes(&mut pool_without, &root_without),
            pool_first_bytes(&mut pool_with, &root_with)
        );
    }

    #[test]
    fn every_pool_stream_rekeys_after_an_advance() {
        let (_, root_without, mut pool_without) = restored_machine();
        let (window, root_with, mut pool_with) = restored_machine();
        window.write(id(2));

        assert_ne!(
            pool_without.insecure_u64(&root_without),
            pool_with.insecure_u64(&root_with)
        );
        let (_, root_without, mut pool_without) = restored_machine();
        let (window, root_with, mut pool_with) = restored_machine();
        window.write(id(2));
        assert_ne!(
            pool_without.insecure_seed(&root_without),
            pool_with.insecure_seed(&root_with)
        );
    }

    #[test]
    fn a_pool_rekeys_once_per_advance() {
        let (window, root, mut pool) = restored_machine();
        window.write(id(2));
        pool_first_bytes(&mut pool, &root);
        let rekeys = root.rekey_count();
        // The pool derived itself from the root, which rekeyed once, and
        // nothing moves after that.
        pool_first_bytes(&mut pool, &root);
        first_bytes(&root);
        assert_eq!(root.rekey_count(), rekeys);
    }

    #[test]
    fn an_image_restore_rekeys_the_root_and_its_pools() {
        let (_, root_without, mut pool_without) = restored_machine();
        let (_, root_with, mut pool_with) = restored_machine();
        root_with
            .generation()
            .advance(AdvanceCause::ImageRestored { image_id: [7; 16] });

        assert_ne!(first_bytes(&root_without), first_bytes(&root_with));
        assert_ne!(
            pool_first_bytes(&mut pool_without, &root_without),
            pool_first_bytes(&mut pool_with, &root_with)
        );
        assert_eq!(root_with.rekey_count(), 1);
    }

    #[test]
    fn different_images_rekey_differently() {
        let (_, first, _) = restored_machine();
        let (_, second, _) = restored_machine();
        first
            .generation()
            .advance(AdvanceCause::ImageRestored { image_id: [7; 16] });
        second
            .generation()
            .advance(AdvanceCause::ImageRestored { image_id: [8; 16] });

        assert_ne!(first_bytes(&first), first_bytes(&second));
    }

    #[test]
    fn the_reseed_task_takes_fresh_material_at_once_after_an_advance() {
        use alloc::sync::Arc;
        use core::future::Future;
        use core::pin::pin;
        use core::sync::atomic::{AtomicUsize, Ordering};
        use core::task::{Context, Poll, Waker};

        use helios_hal::io::IoError;

        use super::{HardwareEntropySource, reseed_forever};

        struct CountingDevice(Arc<AtomicUsize>);

        impl HardwareEntropySource for CountingDevice {
            fn fill<'a>(
                &'a self,
                buffer: &'a mut [u8],
            ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
                self.0.fetch_add(1, Ordering::SeqCst);
                buffer.fill(0xd0);
                core::future::ready(Ok(()))
            }

            fn drain_completions(&self) {}
        }

        let reads = Arc::new(AtomicUsize::new(0));
        let cpu = TestCpu::with_entropy(0x5a);
        let (window, handle) = TestWindow::new(id(1));
        let root =
            RootEntropy::from_platform(&cpu, None, None, MachineGeneration::new(Some(handle)))
                .expect("a CPU source is enough to seed the root");
        let root = super::RootEntropyHandle::new(root);

        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(reseed_forever(
            root.clone(),
            CountingDevice(reads.clone()),
            crate::test_support::test_timer(cpu),
        ));

        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "the first read is at start"
        );
        assert_eq!(root.reseed_count(), 1);

        // Nothing moved, so nothing is read before the interval.
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        window.write(id(2));
        assert_eq!(root.generation().observe().get(), 1);
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "the advance reads the device at once"
        );
        assert_eq!(root.reseed_count(), 2);
    }
}

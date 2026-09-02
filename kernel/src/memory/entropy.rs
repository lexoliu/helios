//! Kernel entropy: one root DRBG per boot, and the per-instance pools
//! derived from it.
//!
//! Every cryptographic byte the kernel or a guest sees comes from
//! [`RootEntropy`], a ChaCha20 DRBG seeded at boot from every source the
//! platform actually has — the CPU's own instruction or host source, the
//! seed firmware handed us in the device tree, and the bytes a
//! virtio-entropy device produces once the executor is running. A kernel
//! that finds none of them fails to boot rather than running with a
//! predictable stream.
//!
//! Concurrency contract: the root DRBG is a spin mutex held for the
//! length of one generate or reseed and never across an await. Reads of
//! the device are the only part that awaits, and they happen in a kernel
//! task outside the lock. Per-instance [`EntropyPool`]s are owned by
//! their instance and take no lock at all.

extern crate alloc;

use alloc::vec::Vec;
use core::future::Future;
use core::time::Duration;

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

use crate::{Kernel, Timer};

/// Bytes the root DRBG draws from a hardware source per seed or reseed.
pub const ROOT_ENTROPY_MATERIAL_BYTES: usize = 64;

/// Domain separators, so material that reaches the hash through two
/// different paths cannot collide into the same key.
const SEED_DOMAIN: &[u8] = b"helios.entropy.root.seed.v1";
const RESEED_DOMAIN: &[u8] = b"helios.entropy.root.reseed.v1";
const DERIVE_DOMAIN: &[u8] = b"helios.entropy.pool.derive.v1";

/// A kernel image that reaches this has no way to produce unpredictable
/// bytes, so it cannot serve `wasi:random/random`, seed an address-space
/// layout, or pick a TCP initial sequence number. Booting on regardless
/// would make every one of those predictable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error(
    "the platform exposes no cryptographic entropy source: neither the CPU, nor firmware, nor a \
     virtio-entropy device offered seed material"
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
    /// A virtio-entropy device has reseeded the root at least once.
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
}

struct RootState {
    drbg: ChaCha20Rng,
    sources: EntropySources,
    reseeds: u64,
    generated: u64,
}

impl RootEntropy {
    /// Seeds the root DRBG from the sources available before the
    /// executor runs: the CPU's own source and whatever seed firmware
    /// left for us.
    ///
    /// Neither is a fallback for the other — every source present is
    /// mixed in, and the result is unpredictable as long as one of them
    /// was. A virtio-entropy device joins later through [`Self::reseed`]
    /// because reading it requires the executor and device interrupts.
    pub fn from_platform<CpuImpl>(
        cpu: &CpuImpl,
        firmware_seed: Option<&[u8]>,
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

        if !sources.any() {
            return Err(NoCryptographicEntropy);
        }

        Ok(Self {
            state: Mutex::new(RootState {
                drbg: ChaCha20Rng::from_seed(hash.finalize().into()),
                sources,
                reseeds: 0,
                generated: 0,
            }),
        })
    }

    /// Mixes fresh hardware material into the root DRBG.
    ///
    /// The current state is folded in alongside the new material, so a
    /// device that turns out to produce poor bytes can only ever add to
    /// what the root already had.
    pub fn reseed(&self, material: &[u8; ROOT_ENTROPY_MATERIAL_BYTES]) {
        let mut state = self.state.lock();
        let mut carry = [0_u8; 32];
        state.drbg.fill_bytes(&mut carry);

        let mut hash = Sha256::new();
        hash.update(RESEED_DOMAIN);
        hash.update(carry);
        hash.update(material);
        state.drbg = ChaCha20Rng::from_seed(hash.finalize().into());
        state.sources.device = true;
        state.reseeds = state.reseeds.saturating_add(1);
        state.generated = 0;
    }

    /// Fills `buffer` from the root DRBG.
    pub fn fill(&self, buffer: &mut [u8]) {
        let mut state = self.state.lock();
        state.drbg.fill_bytes(buffer);
        state.generated = state.generated.saturating_add(buffer.len() as u64);
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
/// Backends call this once on the bootstrap processor with whatever seed
/// firmware left them — the device tree's `/chosen/rng-seed` on the
/// platforms that have one — and install the result on the runtime
/// state. A platform with no cryptographic source at all panics here:
/// the alternative is a kernel whose `wasi:random/random`, address-space
/// layout and TCP sequence numbers are all predictable, which is worse
/// than not booting.
pub fn seed_root_entropy<CpuImpl>(cpu: &CpuImpl, firmware_seed: Option<&[u8]>) -> RootEntropyHandle
where
    CpuImpl: Cpu,
{
    let root =
        RootEntropy::from_platform(cpu, firmware_seed).unwrap_or_else(|error| panic!("{error}"));
    let sources = root.sources();
    tracing::info!(
        cpu = sources.cpu,
        firmware = sources.firmware,
        "root entropy seeded"
    );
    Arc::new(root)
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
    kernel.spawn_detached(async move {
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
    let mut material = [0_u8; ROOT_ENTROPY_MATERIAL_BYTES];
    loop {
        match device.fill(&mut material).await {
            Ok(()) => {
                root.reseed(&material);
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
        timer.sleep_for(ENTROPY_RESEED_INTERVAL).await;
    }
}

/// One instance's random streams.
///
/// A pool is derived from the root DRBG when its instance is created and
/// is owned by that instance afterwards, so drawing from it costs no
/// lock and cannot be observed by another instance.
pub struct EntropyPool {
    secure: ChaCha20Rng,
    insecure: ChaCha20Rng,
    insecure_seed: ChaCha20Rng,
}

impl EntropyPool {
    /// Derives an instance pool from the root DRBG.
    ///
    /// `personalization` separates two instances that draw at the same
    /// moment; the root's own output is what makes the streams
    /// unpredictable.
    pub fn derive(root: &RootEntropy, personalization: u64) -> Self {
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
        }
    }

    pub fn fill_secure(&mut self, buffer: &mut [u8]) {
        self.secure.fill_bytes(buffer);
    }

    pub fn secure_u64(&mut self) -> u64 {
        self.secure.next_u64()
    }

    pub fn insecure_bytes(&mut self, len: usize) -> Vec<u8> {
        let mut bytes = alloc::vec![0_u8; len];
        self.insecure.fill_bytes(&mut bytes);
        bytes
    }

    pub fn insecure_u64(&mut self) -> u64 {
        self.insecure.next_u64()
    }

    pub fn insecure_seed(&mut self) -> (u64, u64) {
        (self.insecure_seed.next_u64(), self.insecure_seed.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EntropyPool, EntropySources, NoCryptographicEntropy, ROOT_ENTROPY_MATERIAL_BYTES,
        RootEntropy,
    };
    use helios_hal::cpu::{Cpu, Instant, ProcessorId};
    use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};

    /// A CPU whose only interesting behaviour is whether it has an
    /// entropy source, and what that source produces.
    #[derive(Clone, Copy)]
    struct TestCpu {
        entropy: Option<u8>,
    }

    impl TestCpu {
        const fn with_entropy(fill: u8) -> Self {
            Self {
                entropy: Some(fill),
            }
        }

        const fn without_entropy() -> Self {
            Self { entropy: None }
        }
    }

    impl Cpu for TestCpu {
        fn current_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn processor_count(&self) -> usize {
            1
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn park_current(&self) {}

        fn start_processor(&self, _: ProcessorId) {}

        fn wake_processor(&self, _: ProcessorId) {}

        fn now(&self) -> Instant {
            Instant::new(11)
        }

        fn timer_frequency(&self) -> u64 {
            1_000_000
        }

        fn set_deadline(&self, _: Instant) {}

        fn publish_executable(&self, _: *const u8, _: usize) {}

        fn unpublish_executable(&self, _: *const u8, _: usize) {}

        fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
            None
        }

        fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
            let fill = self.entropy.ok_or(EntropyUnavailable)?;
            buffer.fill(fill);
            Ok(EntropyQuality::Cryptographic)
        }

        fn shutdown(&self) -> ! {
            panic!("test CPU should not shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU should not reboot")
        }
    }

    fn root() -> RootEntropy {
        RootEntropy::from_platform(&TestCpu::with_entropy(0x5a), None)
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
            RootEntropy::from_platform(&TestCpu::without_entropy(), None).err(),
            Some(NoCryptographicEntropy)
        );
    }

    #[test]
    fn firmware_alone_seeds_the_root() {
        let root = RootEntropy::from_platform(&TestCpu::without_entropy(), Some(&[7_u8; 32]))
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
    fn every_present_source_changes_the_seed() {
        // Firmware is mixed in, not used as a fallback for the CPU: two
        // roots that share a CPU source but differ in the firmware seed
        // must not produce the same stream.
        let cpu_only = RootEntropy::from_platform(&TestCpu::with_entropy(0x5a), None)
            .expect("cpu source seeds the root");
        let with_firmware =
            RootEntropy::from_platform(&TestCpu::with_entropy(0x5a), Some(&[3_u8; 32]))
                .expect("cpu and firmware seed the root");
        let other_firmware =
            RootEntropy::from_platform(&TestCpu::with_entropy(0x5a), Some(&[4_u8; 32]))
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
        let first = RootEntropy::from_platform(&TestCpu::with_entropy(1), None).expect("seeded");
        let second = RootEntropy::from_platform(&TestCpu::with_entropy(2), None).expect("seeded");

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
        first.fill_secure(&mut first_bytes);
        second.fill_secure(&mut second_bytes);

        assert_ne!(first_bytes, [0_u8; 32]);
        assert_ne!(first_bytes, second_bytes);
    }

    #[test]
    fn a_pools_streams_are_independent_of_each_other() {
        let root = root();
        let mut pool = EntropyPool::derive(&root, 7);

        assert_ne!(pool.secure_u64(), pool.insecure_u64());
        assert_ne!(pool.insecure_seed(), (0, 0));
        assert_eq!(pool.insecure_bytes(16).len(), 16);
    }

    #[test]
    fn two_pools_derived_alike_still_differ_because_the_root_advances() {
        let root = root();
        let mut first = EntropyPool::derive(&root, 42);
        let mut second = EntropyPool::derive(&root, 42);

        assert_ne!(first.secure_u64(), second.secure_u64());
    }
}

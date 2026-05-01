extern crate alloc;

use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use helios_hal::entropy::EntropyQuality;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum EntropyError {
    #[error("cryptographic entropy source is unavailable")]
    CryptographicSourceUnavailable,
}

pub struct EntropyPool {
    secure: Option<ChaCha20Rng>,
    insecure: ChaCha20Rng,
    insecure_seed: ChaCha20Rng,
}

impl EntropyPool {
    pub fn from_cpu<CpuImpl>(cpu: &CpuImpl, personalization: u64) -> Self
    where
        CpuImpl: Cpu,
    {
        let mut seed_material = [0_u8; 96];
        if cpu.fill_entropy(&mut seed_material) == Ok(EntropyQuality::Cryptographic) {
            return Self::from_seed_material(seed_material, true);
        }

        let weak_seed = cpu.now().ticks().rotate_left(17)
            ^ personalization.rotate_left(31)
            ^ u64::from(cpu.current_processor().id()).rotate_left(47);
        Self {
            secure: None,
            insecure: ChaCha20Rng::seed_from_u64(weak_seed),
            insecure_seed: ChaCha20Rng::seed_from_u64(weak_seed ^ u64::MAX),
        }
    }

    pub fn fill_secure(&mut self, buffer: &mut [u8]) -> Result<(), EntropyError> {
        let rng = self
            .secure
            .as_mut()
            .ok_or(EntropyError::CryptographicSourceUnavailable)?;
        rng.fill_bytes(buffer);
        Ok(())
    }

    pub fn secure_u64(&mut self) -> Result<u64, EntropyError> {
        let rng = self
            .secure
            .as_mut()
            .ok_or(EntropyError::CryptographicSourceUnavailable)?;
        Ok(rng.next_u64())
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

    fn from_seed_material(seed_material: [u8; 96], secure_enabled: bool) -> Self {
        let mut secure_seed = [0_u8; 32];
        let mut insecure_seed = [0_u8; 32];
        let mut insecure_seed_seed = [0_u8; 32];
        secure_seed.copy_from_slice(&seed_material[0..32]);
        insecure_seed.copy_from_slice(&seed_material[32..64]);
        insecure_seed_seed.copy_from_slice(&seed_material[64..96]);

        Self {
            secure: secure_enabled.then(|| ChaCha20Rng::from_seed(secure_seed)),
            insecure: ChaCha20Rng::from_seed(insecure_seed),
            insecure_seed: ChaCha20Rng::from_seed(insecure_seed_seed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_hal::cpu::{Cpu, Instant, ProcessorId};
    use helios_hal::entropy::EntropyUnavailable;

    #[derive(Clone, Copy)]
    struct SecureCpu;

    impl Cpu for SecureCpu {
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
            for (index, byte) in buffer.iter_mut().enumerate() {
                *byte = (index + 1) as u8;
            }
            Ok(EntropyQuality::Cryptographic)
        }

        fn shutdown(&self) -> ! {
            panic!("test CPU should not shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU should not reboot")
        }
    }

    #[derive(Clone, Copy)]
    struct EntropylessCpu;

    impl Cpu for EntropylessCpu {
        fn current_processor(&self) -> ProcessorId {
            ProcessorId::new(3)
        }

        fn processor_count(&self) -> usize {
            4
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn park_current(&self) {}

        fn start_processor(&self, _: ProcessorId) {}

        fn wake_processor(&self, _: ProcessorId) {}

        fn now(&self) -> Instant {
            Instant::new(99)
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

        fn shutdown(&self) -> ! {
            panic!("test CPU should not shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU should not reboot")
        }
    }

    #[test]
    fn secure_pool_uses_platform_entropy() {
        let mut pool = EntropyPool::from_cpu(&SecureCpu, 7);
        let mut bytes = [0_u8; 32];
        pool.fill_secure(&mut bytes).unwrap();
        assert_ne!(bytes, [0_u8; 32]);
        assert_ne!(pool.secure_u64().unwrap(), pool.insecure_u64());
    }

    #[test]
    fn entropyless_pool_rejects_secure_random() {
        let mut pool = EntropyPool::from_cpu(&EntropylessCpu, 7);
        let mut bytes = [0_u8; 8];
        assert_eq!(
            pool.fill_secure(&mut bytes),
            Err(EntropyError::CryptographicSourceUnavailable)
        );
        assert_ne!(pool.insecure_u64(), 0);
        assert_ne!(pool.insecure_seed(), (0, 0));
    }
}

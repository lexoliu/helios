//! The swap tier's runtime settings.
//!
//! Every figure here is an operational fact that can change without a code
//! change (how much of the pool the compressed store may take, how
//! compressible a page has to be, whether the store may spill to disk), so
//! it is a setting an operator adjusts, not a constant. The pacing
//! mechanism around them (`SWAP_BATCH_BYTES`, `SWAP_TICK`, the scan budget)
//! and the facts the frame size fixes (the granule size, the object header)
//! stay constants in their modules.
//!
//! # Concurrency contract
//!
//! [`PolicyCells`] is shared and holds one atomic per setting.
//!
//! - **Setting** ([`PolicyCells::set`]) runs on any task. It validates the
//!   whole [`SwapPolicy`] first, then publishes each field with a relaxed
//!   store: nothing is ordered against the settings, so a decision that reads
//!   them concurrently sees each field as it was or as it is now, never a
//!   torn value, and may see the new value of one field with the old value of
//!   another for the length of one decision.
//! - **Reading** ([`PolicyCells::load`]) runs on any processor and takes
//!   relaxed loads, once per decision: the owner reads it once at the top of
//!   a pass, a fault never reads it.
//!
//! Nothing here blocks, awaits or allocates.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use core::time::Duration;

use thiserror::Error;

use super::slot::Gate;

/// How long an instance has to have been off every processor before a
/// `Yellow` pass will evict it by default. Ten seconds is long enough that an
/// instance between two guest calls is never mistaken for an idle one, and
/// short enough that a shell left open for a minute pays for itself.
const DEFAULT_IDLE_AFTER: Duration = Duration::from_secs(10);

/// Linux zswap's `max_pool_percent` (`mm/zswap.c`): the share of memory the
/// compressed pool may take.
const DEFAULT_STORE_LIMIT_PERCENT: u8 = 20;

/// Linux zswap's `accept_thr_percent` (`mm/zswap.c`): once the pool has
/// refused for its limit, the share of the limit it must be back down to.
const DEFAULT_STORE_ACCEPT_PERCENT: u8 = 90;

/// A page is accepted when its object takes at most this many 64-byte
/// granules: 2880 bytes, 70.3% of a page, which is Zircon's 70% target for
/// compressed pages (RFC-0219).
const DEFAULT_ACCEPT_GRANULES: u8 = 45;

/// The settings that steer the swap tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapPolicy {
    /// How long an instance has to have been off every processor before a
    /// `Yellow` pass evicts it.
    pub idle_after: Duration,
    /// The share of the user pool the compressed store's objects may take,
    /// in percent, `0..=100`. Zero keeps the store empty.
    pub store_limit_percent: u8,
    /// Once the store has refused an object for its limit, the share of that
    /// limit it has to be back down to before it takes objects again, in
    /// percent, `0..=100`.
    pub store_accept_percent: u8,
    /// The most 64-byte granules an object may take for the store to accept
    /// its page, `1..=63`: a 64-granule object saves nothing.
    pub accept_granules: u8,
    /// Whether the store may write its oldest objects to the disk.
    pub writeback: bool,
}

impl Default for SwapPolicy {
    fn default() -> Self {
        Self {
            idle_after: DEFAULT_IDLE_AFTER,
            store_limit_percent: DEFAULT_STORE_LIMIT_PERCENT,
            store_accept_percent: DEFAULT_STORE_ACCEPT_PERCENT,
            accept_granules: DEFAULT_ACCEPT_GRANULES,
            writeback: true,
        }
    }
}

/// A setting outside the range it can physically take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum SwapPolicyError {
    #[error("the idle delay of {0:?} does not fit the 64-bit nanosecond clock")]
    IdleAfter(Duration),
    #[error("the store limit of {0}% is not a share of the pool: it must be at most 100")]
    StoreLimitPercent(u8),
    #[error("the store accept level of {0}% is not a share of the limit: it must be at most 100")]
    StoreAcceptPercent(u8),
    #[error(
        "{0} granules is not an acceptance bound: an object takes 1 to 63 granules of its frame"
    )]
    AcceptGranules(u8),
}

/// The settings, shared. See the module docs for who reads and writes them.
pub(super) struct PolicyCells {
    idle_after_nanos: AtomicU64,
    store_limit_percent: AtomicU8,
    store_accept_percent: AtomicU8,
    accept_granules: AtomicU8,
    writeback: AtomicBool,
}

impl PolicyCells {
    pub(super) fn new(policy: SwapPolicy) -> Result<Self, SwapPolicyError> {
        let cells = Self {
            idle_after_nanos: AtomicU64::new(0),
            store_limit_percent: AtomicU8::new(0),
            store_accept_percent: AtomicU8::new(0),
            accept_granules: AtomicU8::new(1),
            writeback: AtomicBool::new(false),
        };
        cells.set(policy)?;
        Ok(cells)
    }

    /// Validates `policy` and publishes it. Any task.
    pub(super) fn set(&self, policy: SwapPolicy) -> Result<(), SwapPolicyError> {
        let idle_after = u64::try_from(policy.idle_after.as_nanos())
            .map_err(|_| SwapPolicyError::IdleAfter(policy.idle_after))?;
        if policy.store_limit_percent > 100 {
            return Err(SwapPolicyError::StoreLimitPercent(
                policy.store_limit_percent,
            ));
        }
        if policy.store_accept_percent > 100 {
            return Err(SwapPolicyError::StoreAcceptPercent(
                policy.store_accept_percent,
            ));
        }
        if Gate::new(policy.accept_granules).is_none() {
            return Err(SwapPolicyError::AcceptGranules(policy.accept_granules));
        }
        self.idle_after_nanos.store(idle_after, Ordering::Relaxed);
        self.store_limit_percent
            .store(policy.store_limit_percent, Ordering::Relaxed);
        self.store_accept_percent
            .store(policy.store_accept_percent, Ordering::Relaxed);
        self.accept_granules
            .store(policy.accept_granules, Ordering::Relaxed);
        self.writeback.store(policy.writeback, Ordering::Relaxed);
        Ok(())
    }

    /// The settings as they read now. Any processor.
    pub(super) fn load(&self) -> SwapPolicy {
        SwapPolicy {
            idle_after: Duration::from_nanos(self.idle_after_nanos.load(Ordering::Relaxed)),
            store_limit_percent: self.store_limit_percent.load(Ordering::Relaxed),
            store_accept_percent: self.store_accept_percent.load(Ordering::Relaxed),
            accept_granules: self.accept_granules.load(Ordering::Relaxed),
            writeback: self.writeback.load(Ordering::Relaxed),
        }
    }
}

impl SwapPolicy {
    /// The compressibility gate these settings describe.
    pub(super) fn gate(&self) -> Gate {
        Gate::new(self.accept_granules)
            .expect("a published policy holds an acceptance bound the gate takes")
    }
}

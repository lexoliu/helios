//! The filters `helios:system/tracing` and `helios:system/profiling` take.
//!
//! A filter arrives from a guest as a record holding a `list<string>`.
//! Lifting the generated record would copy the whole list into kernel
//! memory before the host function ran, and the guest sets its size, so a
//! bound checked afterwards would leave a transient kernel allocation the
//! guest controls, and kernel OOM is fatal. The host functions instead take
//! the views below, whose list is a [`WasmList`] of [`WasmStr`]: a window
//! onto the caller's linear memory that has been bounds-checked but not
//! copied. The count is checked against the list's length, each prefix
//! against its decoded length, and only a prefix that fits is copied into
//! the kernel's fixed-size [`PrefixSet`].
//!
//! The two generated worlds spell every shared type identically, so the
//! views are written once and name the WIT records by their component
//! model shape, as the vsock registration does for its value types.

use wasmtime::component::{ComponentType, Lift, WasmList, WasmStr};
use wasmtime::{AsContext, StoreContextMut};

use crate::{
    PerfMetricFilter, PrefixSet, PrefixSetError, ProfileFilter, ProfileScope, TraceFilter,
    TraceLevel,
};

use super::{debugger_wit, program_wit};

/// `helios:system/tracing.level`.
#[derive(ComponentType, Lift)]
#[component(enum)]
#[repr(u8)]
#[expect(
    dead_code,
    reason = "the `Lift` derive builds every variant by transmuting the guest's checked discriminant"
)]
pub(super) enum LevelView {
    #[component(name = "error")]
    Error,
    #[component(name = "warn")]
    Warn,
    #[component(name = "info")]
    Info,
    #[component(name = "debug")]
    Debug,
    #[component(name = "trace")]
    Trace,
}

/// `helios:system/profiling.scope`.
#[derive(ComponentType, Lift)]
#[component(enum)]
#[repr(u8)]
#[expect(
    dead_code,
    reason = "the `Lift` derive builds every variant by transmuting the guest's checked discriminant"
)]
pub(super) enum ScopeView {
    #[component(name = "kernel")]
    Kernel,
    #[component(name = "user")]
    User,
}

/// `helios:system/tracing.filter`.
#[derive(ComponentType, Lift)]
#[component(record)]
pub(super) struct TraceFilterView {
    #[component(name = "min-level")]
    min_level: Option<LevelView>,
    #[component(name = "target-prefixes")]
    target_prefixes: WasmList<WasmStr>,
}

/// `helios:system/profiling.filter`.
#[derive(ComponentType, Lift)]
#[component(record)]
pub(super) struct ProfileFilterView {
    scope: Option<ScopeView>,
    #[component(name = "stack-prefixes")]
    stack_prefixes: WasmList<WasmStr>,
}

/// `helios:system/profiling.metric-filter`.
#[derive(ComponentType, Lift)]
#[component(record)]
pub(super) struct MetricFilterView {
    #[component(name = "name-prefixes")]
    name_prefixes: WasmList<WasmStr>,
}

/// What a filter lifts to: the kernel's own filter, or the bound it broke.
///
/// The outer `wasmtime::Result` is the guest handing over memory that is
/// not a valid list or string, which traps; the inner one is a filter the
/// kernel refuses, which the guest is told about.
pub(super) type Lifted<F> = wasmtime::Result<Result<F, PrefixSetError>>;

impl TraceFilterView {
    pub(super) fn lift<T: 'static>(
        self,
        store: &mut StoreContextMut<'_, T>,
    ) -> Lifted<TraceFilter> {
        Ok(
            lift_prefixes(&self.target_prefixes, store)?.map(|target_prefixes| TraceFilter {
                min_level: self.min_level.map(TraceLevel::from),
                target_prefixes,
            }),
        )
    }
}

impl ProfileFilterView {
    pub(super) fn lift<T: 'static>(
        self,
        store: &mut StoreContextMut<'_, T>,
    ) -> Lifted<ProfileFilter> {
        Ok(
            lift_prefixes(&self.stack_prefixes, store)?.map(|stack_prefixes| ProfileFilter {
                scope: self.scope.map(ProfileScope::from),
                stack_prefixes,
            }),
        )
    }
}

impl MetricFilterView {
    pub(super) fn lift<T: 'static>(
        self,
        store: &mut StoreContextMut<'_, T>,
    ) -> Lifted<PerfMetricFilter> {
        Ok(lift_prefixes(&self.name_prefixes, store)?
            .map(|name_prefixes| PerfMetricFilter { name_prefixes }))
    }
}

/// Copies `prefixes` into a [`PrefixSet`] once it is known to fit.
///
/// The count is decided from the list's length alone, before any element
/// is read. A prefix's length is decided from the decoded text, which for
/// a UTF-8 string is a borrow of the caller's memory, so the copy happens
/// only for a prefix that fits.
fn lift_prefixes<T: 'static>(
    prefixes: &WasmList<WasmStr>,
    store: &mut StoreContextMut<'_, T>,
) -> Lifted<PrefixSet> {
    if let Err(refusal) = PrefixSet::admit_count(prefixes.len()) {
        return Ok(Err(refusal));
    }
    let mut set = PrefixSet::default();
    for index in 0..prefixes.len() {
        let prefix = prefixes
            .get(&mut *store, index)
            .ok_or_else(|| wasmtime::Error::msg("a prefix index was past the end of its list"))??;
        let text = prefix.to_str(store.as_context())?;
        if let Err(refusal) = set.push(&text) {
            return Ok(Err(refusal));
        }
    }
    Ok(Ok(set))
}

impl From<LevelView> for TraceLevel {
    fn from(level: LevelView) -> Self {
        match level {
            LevelView::Error => TraceLevel::Error,
            LevelView::Warn => TraceLevel::Warn,
            LevelView::Info => TraceLevel::Info,
            LevelView::Debug => TraceLevel::Debug,
            LevelView::Trace => TraceLevel::Trace,
        }
    }
}

impl From<ScopeView> for ProfileScope {
    fn from(scope: ScopeView) -> Self {
        match scope {
            ScopeView::Kernel => ProfileScope::Kernel,
            ScopeView::User => ProfileScope::User,
        }
    }
}

/// `filter-error` names the limit that was exceeded in its payload. Both
/// limits are small constants, so the casts never truncate.
macro_rules! impl_filter_error_conversion {
    ($wit:ident) => {
        impl From<PrefixSetError> for $wit::filters::FilterError {
            fn from(refusal: PrefixSetError) -> Self {
                match refusal {
                    PrefixSetError::TooManyPrefixes => {
                        Self::TooManyPrefixes(crate::MAX_FILTER_PREFIXES as u32)
                    }
                    PrefixSetError::PrefixTooLong => {
                        Self::PrefixTooLong(crate::MAX_FILTER_PREFIX_BYTES as u32)
                    }
                }
            }
        }
    };
}

impl_filter_error_conversion!(debugger_wit);
impl_filter_error_conversion!(program_wit);

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest that is refused learns the limit, in both worlds' own
    /// `filter-error` types.
    #[test]
    fn a_refusal_names_the_limit_it_exceeded() {
        let count = crate::MAX_FILTER_PREFIXES as u32;
        let bytes = crate::MAX_FILTER_PREFIX_BYTES as u32;

        assert!(matches!(
            debugger_wit::filters::FilterError::from(PrefixSetError::TooManyPrefixes),
            debugger_wit::filters::FilterError::TooManyPrefixes(limit) if limit == count
        ));
        assert!(matches!(
            debugger_wit::filters::FilterError::from(PrefixSetError::PrefixTooLong),
            debugger_wit::filters::FilterError::PrefixTooLong(limit) if limit == bytes
        ));
        assert!(matches!(
            program_wit::filters::FilterError::from(PrefixSetError::TooManyPrefixes),
            program_wit::filters::FilterError::TooManyPrefixes(limit) if limit == count
        ));
        assert!(matches!(
            program_wit::filters::FilterError::from(PrefixSetError::PrefixTooLong),
            program_wit::filters::FilterError::PrefixTooLong(limit) if limit == bytes
        ));
    }
}

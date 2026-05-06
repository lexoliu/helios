//! Profiling syscalls.

use crate::bindings::helios::system::profiling as raw;
pub use crate::bindings::helios::system::profiling::{
    Filter, FoldedSample, MetricFilter, MetricSample, MonoNanos, Scope,
};

pub fn set_enabled(enabled: bool) {
    raw::set_enabled(enabled);
}

pub fn clear() {
    raw::clear();
}

pub fn folded(filter: &Filter, limit: u32) -> Vec<FoldedSample> {
    raw::folded(filter, limit)
}

pub fn metrics(filter: &MetricFilter, limit: u32) -> Vec<MetricSample> {
    raw::metrics(filter, limit)
}

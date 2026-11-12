extern crate alloc;

use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Reverse;

use heapless::binary_heap::{BinaryHeap as InlineBinaryHeap, Min};
use helios_hal::resource::{KernelResource, ResourceRights};
use thiserror::Error;

const INLINE_FREE_DESCRIPTOR_SLOTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DescriptorId(u32);

impl DescriptorId {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Clone)]
pub struct DescriptorEntry<T, Rights>
where
    Rights: ResourceRights,
{
    resource: KernelResource<T, Rights>,
    close_on_exec: bool,
}

impl<T, Rights> DescriptorEntry<T, Rights>
where
    Rights: ResourceRights,
{
    pub fn new(resource: KernelResource<T, Rights>, close_on_exec: bool) -> Self {
        Self {
            resource,
            close_on_exec,
        }
    }

    pub fn resource(&self) -> &KernelResource<T, Rights> {
        &self.resource
    }

    pub fn close_on_exec(&self) -> bool {
        self.close_on_exec
    }

    pub fn set_close_on_exec(&mut self, close_on_exec: bool) {
        self.close_on_exec = close_on_exec;
    }

    pub fn into_resource(self) -> KernelResource<T, Rights> {
        self.resource
    }
}

/// How many descriptor numbers a table hands out: a descriptor number is
/// valid only below the limit, which is what bounds the table's storage.
/// It is an operator setting; the table's owner sets it, and nothing here
/// caps the value it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorLimit(usize);

impl DescriptorLimit {
    /// 1024 is the soft `RLIMIT_NOFILE` a new process starts with on Linux,
    /// the system the POSIX guest programs are ported from.
    pub const DEFAULT: Self = Self(1024);

    pub const fn new(limit: usize) -> Self {
        Self(limit)
    }

    pub const fn get(self) -> usize {
        self.0
    }

    /// Whether the descriptor number `index` lies below the limit.
    pub const fn admits(self, index: usize) -> bool {
        index < self.0
    }
}

impl Default for DescriptorLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DescriptorTableError {
    /// The descriptor is not open, or its number is at or past the limit
    /// (`EBADF`).
    #[error("bad descriptor {0}")]
    BadDescriptor(u32),
    #[error("descriptor rights cannot be widened")]
    RightsWidening,
    #[error("descriptor index overflows usize")]
    IndexOverflow,
    /// No descriptor number below the limit is free (`EMFILE`).
    #[error("no free descriptor below the limit of {limit}")]
    TooManyDescriptors { limit: usize },
}

#[derive(Clone)]
pub struct DescriptorTable<T, Rights>
where
    Rights: ResourceRights,
{
    entries: Vec<Option<DescriptorEntry<T, Rights>>>,
    free: FreeDescriptorSlots,
    limit: DescriptorLimit,
}

impl<T, Rights> Default for DescriptorTable<T, Rights>
where
    Rights: ResourceRights,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T, Rights> DescriptorTable<T, Rights>
where
    Rights: ResourceRights,
{
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: FreeDescriptorSlots::new(),
            limit: DescriptorLimit::DEFAULT,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            free: FreeDescriptorSlots::with_capacity(capacity),
            limit: DescriptorLimit::DEFAULT,
        }
    }

    pub const fn limit(&self) -> DescriptorLimit {
        self.limit
    }

    /// Sets the descriptor limit. Descriptors already open at or past a
    /// lowered limit stay open; no new number at or past it is handed out.
    pub fn set_limit(&mut self, limit: DescriptorLimit) {
        self.limit = limit;
    }

    pub fn insert(
        &mut self,
        resource: KernelResource<T, Rights>,
        close_on_exec: bool,
    ) -> Result<DescriptorId, DescriptorTableError> {
        let index = self.allocate_slot_index()?;
        let id = u32::try_from(index).map_err(|_| DescriptorTableError::IndexOverflow)?;
        self.entries[index] = Some(DescriptorEntry::new(resource, close_on_exec));
        Ok(DescriptorId::new(id))
    }

    pub fn get(
        &self,
        descriptor: DescriptorId,
    ) -> Result<&DescriptorEntry<T, Rights>, DescriptorTableError> {
        self.entries
            .get(descriptor_index(descriptor)?)
            .and_then(Option::as_ref)
            .ok_or(DescriptorTableError::BadDescriptor(descriptor.raw()))
    }

    pub fn get_mut(
        &mut self,
        descriptor: DescriptorId,
    ) -> Result<&mut DescriptorEntry<T, Rights>, DescriptorTableError> {
        self.entries
            .get_mut(descriptor_index(descriptor)?)
            .and_then(Option::as_mut)
            .ok_or(DescriptorTableError::BadDescriptor(descriptor.raw()))
    }

    pub fn close(
        &mut self,
        descriptor: DescriptorId,
    ) -> Result<KernelResource<T, Rights>, DescriptorTableError> {
        let resource = self
            .entries
            .get_mut(descriptor_index(descriptor)?)
            .and_then(Option::take)
            .map(DescriptorEntry::into_resource)
            .ok_or(DescriptorTableError::BadDescriptor(descriptor.raw()))?;
        self.free.release(descriptor_index(descriptor)?);
        Ok(resource)
    }

    pub fn dup(
        &mut self,
        descriptor: DescriptorId,
        rights: Rights,
        close_on_exec: bool,
    ) -> Result<DescriptorId, DescriptorTableError> {
        let resource = self
            .get(descriptor)?
            .resource()
            .derive(rights)
            .ok_or(DescriptorTableError::RightsWidening)?;
        self.insert(resource, close_on_exec)
    }

    pub fn renumber(
        &mut self,
        source: DescriptorId,
        target: DescriptorId,
    ) -> Result<(), DescriptorTableError> {
        if source == target {
            self.get(source)?;
            return Ok(());
        }

        let source_index = descriptor_index(source)?;
        let target_index = descriptor_index(target)?;
        if !self.limit.admits(target_index) {
            return Err(DescriptorTableError::BadDescriptor(target.raw()));
        }
        let entry = self
            .entries
            .get_mut(source_index)
            .and_then(Option::take)
            .ok_or(DescriptorTableError::BadDescriptor(source.raw()))?;

        if self.entries.len() <= target_index {
            let old_len = self.entries.len();
            self.entries.resize_with(target_index + 1, || None);
            self.free.release_range(old_len..target_index);
        }
        self.entries[target_index] = Some(entry);
        self.free.release(source_index);
        Ok(())
    }

    pub fn close_on_exec(&mut self) {
        for (index, slot) in self.entries.iter_mut().enumerate() {
            if slot.as_ref().is_some_and(DescriptorEntry::close_on_exec) {
                *slot = None;
                self.free.release(index);
            }
        }
    }

    /// The lowest free descriptor number below the limit, or
    /// `TooManyDescriptors`; nothing is allocated on failure.
    fn allocate_slot_index(&mut self) -> Result<usize, DescriptorTableError> {
        while let Some(index) = self.free.allocate() {
            if !self.limit.admits(index) {
                // The heap pops the lowest number, so every other free
                // number is at or past the limit too. Keep this one free
                // for when the limit is raised.
                self.free.release(index);
                break;
            }
            if self.entries.get(index).is_some_and(Option::is_none) {
                return Ok(index);
            }
        }
        let index = self.entries.len();
        if !self.limit.admits(index) {
            return Err(DescriptorTableError::TooManyDescriptors {
                limit: self.limit.get(),
            });
        }
        self.entries.push(None);
        Ok(index)
    }
}

#[derive(Clone)]
pub(crate) struct FreeDescriptorSlots {
    inline: InlineBinaryHeap<usize, Min, INLINE_FREE_DESCRIPTOR_SLOTS>,
    overflow: BinaryHeap<Reverse<usize>>,
}

impl FreeDescriptorSlots {
    pub(crate) const fn new() -> Self {
        Self {
            inline: InlineBinaryHeap::new(),
            overflow: BinaryHeap::new(),
        }
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut slots = Self::new();
        if capacity > INLINE_FREE_DESCRIPTOR_SLOTS {
            slots
                .overflow
                .reserve_exact(capacity - INLINE_FREE_DESCRIPTOR_SLOTS);
        }
        slots
    }

    pub(crate) fn release(&mut self, index: usize) {
        if let Err(index) = self.inline.push(index) {
            self.overflow.push(Reverse(index));
        }
    }

    pub(crate) fn release_range(&mut self, range: core::ops::Range<usize>) {
        if range.len() > INLINE_FREE_DESCRIPTOR_SLOTS {
            self.overflow.reserve_exact(range.len());
            self.overflow.extend(range.map(Reverse));
            return;
        }
        for index in range {
            self.release(index);
        }
    }

    pub(crate) fn allocate(&mut self) -> Option<usize> {
        match (
            self.inline.peek().copied(),
            self.overflow.peek().map(|Reverse(index)| *index),
        ) {
            (Some(inline), Some(overflow)) if inline <= overflow => self.inline.pop(),
            (Some(_), None) => self.inline.pop(),
            (_, Some(_)) => self.overflow.pop().map(|Reverse(index)| index),
            (None, None) => None,
        }
    }
}

fn descriptor_index(descriptor: DescriptorId) -> Result<usize, DescriptorTableError> {
    usize::try_from(descriptor.raw()).map_err(|_| DescriptorTableError::IndexOverflow)
}

#[cfg(test)]
mod tests {
    use super::{DescriptorId, DescriptorLimit, DescriptorTable, DescriptorTableError};
    use helios_hal::fs::FileRights;
    use helios_hal::resource::KernelResource;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestFile(&'static str);

    fn file(rights: FileRights) -> KernelResource<TestFile, FileRights> {
        KernelResource::new(TestFile("file"), rights)
    }

    #[test]
    fn dup_can_lower_but_not_widen_rights() {
        let mut table = DescriptorTable::new();
        let fd = table
            .insert(file(FileRights::READ | FileRights::WRITE), false)
            .expect("insert must allocate descriptor");

        let readonly = table
            .dup(fd, FileRights::READ, false)
            .expect("dup may lower rights");
        assert!(table.get(readonly).unwrap().resource().rights() == FileRights::READ);

        let error = table
            .dup(readonly, FileRights::READ | FileRights::WRITE, false)
            .expect_err("dup must reject rights widening");
        assert_eq!(error, DescriptorTableError::RightsWidening);
    }

    #[test]
    fn renumber_moves_descriptor_and_closes_target() {
        let mut table = DescriptorTable::new();
        let source = table
            .insert(file(FileRights::READ), false)
            .expect("source insert must succeed");
        let target = table
            .insert(file(FileRights::WRITE), false)
            .expect("target insert must succeed");

        table
            .renumber(source, target)
            .expect("renumber must move descriptor");

        assert!(matches!(
            table.get(source),
            Err(DescriptorTableError::BadDescriptor(_))
        ));
        assert!(table.get(target).unwrap().resource().rights() == FileRights::READ);
    }

    #[test]
    fn close_on_exec_drops_only_marked_descriptors() {
        let mut table = DescriptorTable::new();
        let keep = table
            .insert(file(FileRights::READ), false)
            .expect("keep insert must succeed");
        let close = table
            .insert(file(FileRights::WRITE), true)
            .expect("close insert must succeed");

        table.close_on_exec();

        assert!(table.get(keep).is_ok());
        assert!(matches!(
            table.get(close),
            Err(DescriptorTableError::BadDescriptor(_))
        ));
    }

    #[test]
    fn insert_reuses_lowest_closed_descriptor() {
        let mut table = DescriptorTable::new();
        let first = table
            .insert(file(FileRights::READ), false)
            .expect("first insert must succeed");
        let second = table
            .insert(file(FileRights::WRITE), false)
            .expect("second insert must succeed");
        let third = table
            .insert(file(FileRights::READ | FileRights::WRITE), false)
            .expect("third insert must succeed");

        table.close(third).expect("third close must succeed");
        table.close(first).expect("first close must succeed");

        let reused = table
            .insert(file(FileRights::READ), false)
            .expect("insert must reuse a closed descriptor");
        assert_eq!(reused.raw(), first.raw());
        assert!(table.get(second).is_ok());
    }

    #[test]
    fn renumber_to_sparse_target_allocates_target_slot() {
        let mut table = DescriptorTable::new();
        let source = table
            .insert(file(FileRights::READ), false)
            .expect("source insert must succeed");
        let target = DescriptorId::new(9);

        table
            .renumber(source, target)
            .expect("renumber to sparse target must succeed");

        assert!(table.get(target).unwrap().resource().rights() == FileRights::READ);
    }

    #[test]
    fn renumber_to_sparse_target_reuses_lowest_intermediate_slot() {
        let mut table = DescriptorTable::new();
        let source = table
            .insert(file(FileRights::READ), false)
            .expect("source insert must succeed");
        let target = DescriptorId::new(9);

        table
            .renumber(source, target)
            .expect("renumber to sparse target must succeed");
        let reused = table
            .insert(file(FileRights::WRITE), false)
            .expect("insert should reuse a sparse free slot");

        assert_eq!(reused.raw(), source.raw());
        assert!(table.get(target).unwrap().resource().rights() == FileRights::READ);
    }

    #[test]
    fn renumber_past_the_limit_is_bad_descriptor_and_allocates_nothing() {
        let mut table = DescriptorTable::new();
        let source = table
            .insert(file(FileRights::READ), false)
            .expect("source insert must succeed");
        let target = DescriptorId::new(u32::MAX - 1);

        assert_eq!(
            table.renumber(source, target),
            Err(DescriptorTableError::BadDescriptor(target.raw()))
        );
        assert!(table.get(source).is_ok());
        assert_eq!(table.entries.len(), 1);
    }

    #[test]
    fn renumber_to_the_last_slot_below_the_limit_succeeds() {
        let mut table = DescriptorTable::new();
        table.set_limit(DescriptorLimit::new(16));
        let source = table
            .insert(file(FileRights::READ), false)
            .expect("source insert must succeed");

        table
            .renumber(source, DescriptorId::new(15))
            .expect("the last number below the limit is valid");
        assert_eq!(
            table.renumber(DescriptorId::new(15), DescriptorId::new(16)),
            Err(DescriptorTableError::BadDescriptor(16))
        );
    }

    #[test]
    fn insert_at_the_limit_is_too_many_descriptors() {
        let mut table = DescriptorTable::new();
        table.set_limit(DescriptorLimit::new(2));
        let first = table
            .insert(file(FileRights::READ), false)
            .expect("first insert must succeed");
        table
            .insert(file(FileRights::READ), false)
            .expect("second insert must succeed");

        let full = DescriptorTableError::TooManyDescriptors { limit: 2 };
        assert_eq!(
            table.insert(file(FileRights::READ), false).unwrap_err(),
            full.clone()
        );
        assert_eq!(table.dup(first, FileRights::READ, false).unwrap_err(), full);
        assert_eq!(table.entries.len(), 2);

        table.close(first).expect("close must succeed");
        let reused = table
            .insert(file(FileRights::READ), false)
            .expect("a closed number below the limit is free again");
        assert_eq!(reused.raw(), first.raw());
    }

    #[test]
    fn lowered_limit_keeps_open_descriptors_and_hands_out_no_number_past_it() {
        let mut table = DescriptorTable::new();
        for _ in 0..4 {
            table
                .insert(file(FileRights::READ), false)
                .expect("insert must succeed");
        }
        table
            .close(DescriptorId::new(3))
            .expect("close must succeed");
        table.set_limit(DescriptorLimit::new(2));

        assert!(table.get(DescriptorId::new(2)).is_ok());
        assert_eq!(
            table.insert(file(FileRights::READ), false).unwrap_err(),
            DescriptorTableError::TooManyDescriptors { limit: 2 }
        );

        table.set_limit(DescriptorLimit::new(8));
        let reused = table
            .insert(file(FileRights::READ), false)
            .expect("raising the limit frees the number again");
        assert_eq!(reused.raw(), 3);
    }
}

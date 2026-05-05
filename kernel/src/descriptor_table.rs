extern crate alloc;

use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Reverse;

use helios_hal::resource::{KernelResource, ResourceRights};
use thiserror::Error;

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

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DescriptorTableError {
    #[error("bad descriptor {0}")]
    BadDescriptor(u32),
    #[error("descriptor rights cannot be widened")]
    RightsWidening,
    #[error("descriptor index overflows usize")]
    IndexOverflow,
}

#[derive(Clone)]
pub struct DescriptorTable<T, Rights>
where
    Rights: ResourceRights,
{
    entries: Vec<Option<DescriptorEntry<T, Rights>>>,
    free: BinaryHeap<Reverse<usize>>,
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
            free: BinaryHeap::new(),
        }
    }

    pub fn insert(
        &mut self,
        resource: KernelResource<T, Rights>,
        close_on_exec: bool,
    ) -> Result<DescriptorId, DescriptorTableError> {
        let index = self.allocate_slot_index();
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
        self.free.push(Reverse(descriptor_index(descriptor)?));
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
        let entry = self
            .entries
            .get_mut(source_index)
            .and_then(Option::take)
            .ok_or(DescriptorTableError::BadDescriptor(source.raw()))?;

        if self.entries.len() <= target_index {
            let previous_len = self.entries.len();
            self.entries.resize_with(target_index + 1, || None);
            for index in previous_len..target_index {
                self.free.push(Reverse(index));
            }
        }
        self.free.push(Reverse(source_index));
        self.entries[target_index] = Some(entry);
        Ok(())
    }

    pub fn close_on_exec(&mut self) {
        for (index, slot) in self.entries.iter_mut().enumerate() {
            if slot.as_ref().is_some_and(DescriptorEntry::close_on_exec) {
                *slot = None;
                self.free.push(Reverse(index));
            }
        }
    }

    fn allocate_slot_index(&mut self) -> usize {
        while let Some(Reverse(index)) = self.free.pop() {
            if self.entries.get(index).is_some_and(Option::is_none) {
                return index;
            }
        }
        let index = self.entries.len();
        self.entries.push(None);
        index
    }
}

fn descriptor_index(descriptor: DescriptorId) -> Result<usize, DescriptorTableError> {
    usize::try_from(descriptor.raw()).map_err(|_| DescriptorTableError::IndexOverflow)
}

#[cfg(test)]
mod tests {
    use super::{DescriptorId, DescriptorTable, DescriptorTableError};
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
}

use alloc::vec::Vec;

use thiserror::Error;

use crate::{
    ExecAuthority, ForkAuthority, JoinAuthority, ProcessAuthority, SignalAuthority, SpawnAuthority,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessId(u64);

impl ProcessId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    Exited(u32),
    Signaled(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRecord {
    id: ProcessId,
    parent: Option<ProcessId>,
    authority: ProcessAuthority,
    state: ProcessState,
}

impl ProcessRecord {
    pub fn id(&self) -> ProcessId {
        self.id
    }

    pub fn parent(&self) -> Option<ProcessId> {
        self.parent
    }

    pub fn authority(&self) -> &ProcessAuthority {
        &self.authority
    }

    pub fn state(&self) -> ProcessState {
        self.state
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProcessTableError {
    #[error("unknown process id {0}")]
    UnknownProcess(u64),
    #[error("child authority exceeds caller authority")]
    AuthorityWidening,
    #[error("process id allocation overflowed")]
    ProcessIdOverflow,
}

#[derive(Default)]
pub struct ProcessTable {
    next_id: u64,
    records: Vec<ProcessRecord>,
}

impl ProcessTable {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            records: Vec::new(),
        }
    }

    pub fn register_root(
        &mut self,
        authority: ProcessAuthority,
    ) -> Result<ProcessId, ProcessTableError> {
        self.insert(None, authority)
    }

    pub fn spawn(
        &mut self,
        _: &SpawnAuthority,
        caller: ProcessId,
        child_authority: ProcessAuthority,
    ) -> Result<ProcessId, ProcessTableError> {
        let caller_authority = self.record(caller)?.authority.clone();
        if !caller_authority.contains_authority(&child_authority) {
            return Err(ProcessTableError::AuthorityWidening);
        }
        self.insert(Some(caller), child_authority)
    }

    pub fn fork(
        &mut self,
        _: &ForkAuthority,
        parent: ProcessId,
    ) -> Result<ProcessId, ProcessTableError> {
        let authority = self.record(parent)?.authority.clone();
        self.insert(Some(parent), authority)
    }

    pub fn exec(&mut self, _: &ExecAuthority, process: ProcessId) -> Result<(), ProcessTableError> {
        self.record_mut(process)?.state = ProcessState::Running;
        Ok(())
    }

    pub fn exit(&mut self, process: ProcessId, code: u32) -> Result<(), ProcessTableError> {
        self.record_mut(process)?.state = ProcessState::Exited(code);
        Ok(())
    }

    pub fn join(
        &self,
        _: &JoinAuthority,
        process: ProcessId,
    ) -> Result<Option<u32>, ProcessTableError> {
        Ok(match self.record(process)?.state {
            ProcessState::Exited(code) => Some(code),
            ProcessState::Running | ProcessState::Signaled(_) => None,
        })
    }

    pub fn signal(
        &mut self,
        _: &SignalAuthority,
        process: ProcessId,
        signal: u32,
    ) -> Result<(), ProcessTableError> {
        self.record_mut(process)?.state = ProcessState::Signaled(signal);
        Ok(())
    }

    pub fn record(&self, process: ProcessId) -> Result<&ProcessRecord, ProcessTableError> {
        self.records
            .iter()
            .find(|record| record.id == process)
            .ok_or(ProcessTableError::UnknownProcess(process.raw()))
    }

    fn record_mut(&mut self, process: ProcessId) -> Result<&mut ProcessRecord, ProcessTableError> {
        self.records
            .iter_mut()
            .find(|record| record.id == process)
            .ok_or(ProcessTableError::UnknownProcess(process.raw()))
    }

    fn insert(
        &mut self,
        parent: Option<ProcessId>,
        authority: ProcessAuthority,
    ) -> Result<ProcessId, ProcessTableError> {
        let id = ProcessId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(ProcessTableError::ProcessIdOverflow)?;
        self.records.push(ProcessRecord {
            id,
            parent,
            authority,
            state: ProcessState::Running,
        });
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirectoryAuthorityRights, DirectoryPreopen, ProcessAuthorityRights};

    fn authority_with(rights: ProcessAuthorityRights) -> ProcessAuthority {
        let mut authority = ProcessAuthority::empty();
        authority.grant_process_rights(rights);
        authority
    }

    #[test]
    fn spawn_requires_child_authority_subset() {
        let mut table = ProcessTable::new();
        let mut parent_authority = authority_with(ProcessAuthorityRights::SPAWN);
        parent_authority.insert_directory_preopen(
            DirectoryPreopen::new("/bin", "/", DirectoryAuthorityRights::READ)
                .expect("test preopen should be valid"),
        );
        let spawn = parent_authority
            .derive_spawn_authority()
            .expect("parent should derive spawn authority");
        let parent = table
            .register_root(parent_authority)
            .expect("root registration should succeed");

        let mut child_authority = ProcessAuthority::empty();
        child_authority.insert_directory_preopen(
            DirectoryPreopen::new("/bin", "/", DirectoryAuthorityRights::READ)
                .expect("test preopen should be valid"),
        );
        let child = table
            .spawn(&spawn, parent, child_authority)
            .expect("subset child authority should spawn");
        assert_eq!(table.record(child).unwrap().parent(), Some(parent));

        let mut overgrant = ProcessAuthority::empty();
        overgrant.insert_directory_preopen(
            DirectoryPreopen::new("/bin", "/", DirectoryAuthorityRights::WRITE)
                .expect("test preopen should be valid"),
        );
        assert_eq!(
            table.spawn(&spawn, parent, overgrant),
            Err(ProcessTableError::AuthorityWidening)
        );
    }

    #[test]
    fn spawn_with_empty_child_authority_has_zero_ambient_rights() {
        let mut table = ProcessTable::new();
        let parent_authority = ProcessAuthority::root();
        let spawn = parent_authority
            .derive_spawn_authority()
            .expect("root should derive spawn authority");
        let parent = table
            .register_root(parent_authority)
            .expect("root registration should succeed");

        let child = table
            .spawn(&spawn, parent, ProcessAuthority::empty())
            .expect("empty child authority should be a valid subset");
        assert_eq!(
            table.record(child).unwrap().authority(),
            &ProcessAuthority::empty()
        );
    }

    #[test]
    fn fork_inherits_and_exec_preserves_authority() {
        let mut table = ProcessTable::new();
        let authority = authority_with(ProcessAuthorityRights::FORK | ProcessAuthorityRights::EXEC);
        let fork = authority
            .derive_fork_authority()
            .expect("parent should derive fork authority");
        let exec = authority
            .derive_exec_authority()
            .expect("parent should derive exec authority");
        let parent = table
            .register_root(authority.clone())
            .expect("root registration should succeed");
        let child = table
            .fork(&fork, parent)
            .expect("fork should inherit parent authority");

        assert_eq!(table.record(child).unwrap().authority(), &authority);
        table
            .exec(&exec, child)
            .expect("exec should preserve authority");
        assert_eq!(table.record(child).unwrap().authority(), &authority);
    }

    #[test]
    fn join_and_signal_require_typed_caps() {
        let mut table = ProcessTable::new();
        let authority =
            authority_with(ProcessAuthorityRights::JOIN | ProcessAuthorityRights::SIGNAL);
        let join = authority
            .derive_join_authority()
            .expect("join authority should derive");
        let signal = authority
            .derive_signal_authority()
            .expect("signal authority should derive");
        let process = table
            .register_root(authority)
            .expect("root registration should succeed");

        assert_eq!(table.join(&join, process).unwrap(), None);
        table.signal(&signal, process, 9).unwrap();
        assert_eq!(
            table.record(process).unwrap().state(),
            ProcessState::Signaled(9)
        );
        assert_eq!(table.join(&join, process).unwrap(), None);
        table.exit(process, 7).unwrap();
        assert_eq!(table.join(&join, process).unwrap(), Some(7));
    }
}

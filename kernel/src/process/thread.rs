use alloc::vec::Vec;

use thiserror::Error;

use crate::ProcessId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(u64);

impl ThreadId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Runnable,
    Sleeping { wake_at_nanos: u64 },
    Exited(u32),
    Signaled(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRecord {
    id: ThreadId,
    process: ProcessId,
    state: ThreadState,
}

impl ThreadRecord {
    pub fn id(&self) -> ThreadId {
        self.id
    }

    pub fn process(&self) -> ProcessId {
        self.process
    }

    pub fn state(&self) -> ThreadState {
        self.state
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ThreadTableError {
    #[error("unknown thread id {0}")]
    UnknownThread(u64),
    #[error("thread id allocation overflowed")]
    ThreadIdOverflow,
}

#[derive(Default)]
pub struct ThreadTable {
    next_id: u64,
    records: Vec<ThreadRecord>,
}

impl ThreadTable {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            records: Vec::new(),
        }
    }

    pub fn spawn(&mut self, process: ProcessId) -> Result<ThreadId, ThreadTableError> {
        let id = ThreadId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(ThreadTableError::ThreadIdOverflow)?;
        self.records.push(ThreadRecord {
            id,
            process,
            state: ThreadState::Runnable,
        });
        Ok(id)
    }

    pub fn set_sleeping(
        &mut self,
        thread: ThreadId,
        wake_at_nanos: u64,
    ) -> Result<(), ThreadTableError> {
        self.record_mut(thread)?.state = ThreadState::Sleeping { wake_at_nanos };
        Ok(())
    }

    pub fn wake_due(&mut self, now_nanos: u64) -> usize {
        let mut woken = 0;
        for record in &mut self.records {
            if let ThreadState::Sleeping { wake_at_nanos } = record.state
                && wake_at_nanos <= now_nanos
            {
                record.state = ThreadState::Runnable;
                woken += 1;
            }
        }
        woken
    }

    pub fn signal(&mut self, thread: ThreadId, signal: u32) -> Result<(), ThreadTableError> {
        self.record_mut(thread)?.state = ThreadState::Signaled(signal);
        Ok(())
    }

    pub fn exit(&mut self, thread: ThreadId, code: u32) -> Result<(), ThreadTableError> {
        self.record_mut(thread)?.state = ThreadState::Exited(code);
        Ok(())
    }

    pub fn join(&self, thread: ThreadId) -> Result<Option<u32>, ThreadTableError> {
        Ok(match self.record(thread)?.state {
            ThreadState::Exited(code) => Some(code),
            ThreadState::Runnable | ThreadState::Sleeping { .. } | ThreadState::Signaled(_) => None,
        })
    }

    pub fn record(&self, thread: ThreadId) -> Result<&ThreadRecord, ThreadTableError> {
        self.records
            .iter()
            .find(|record| record.id == thread)
            .ok_or(ThreadTableError::UnknownThread(thread.raw()))
    }

    pub fn process_parallelism(&self, process: ProcessId) -> usize {
        self.records
            .iter()
            .filter(|record| record.process == process)
            .filter(|record| !matches!(record.state, ThreadState::Exited(_)))
            .count()
    }

    fn record_mut(&mut self, thread: ThreadId) -> Result<&mut ThreadRecord, ThreadTableError> {
        self.records
            .iter_mut()
            .find(|record| record.id == thread)
            .ok_or(ThreadTableError::UnknownThread(thread.raw()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProcessAuthority, ProcessAuthorityRights, ProcessTable};

    fn process(raw: u64) -> ProcessId {
        let mut table = ProcessTable::new();
        let mut authority = ProcessAuthority::empty();
        authority.grant_process_rights(ProcessAuthorityRights::SPAWN);
        let process = table.register_root(authority).unwrap();
        assert_eq!(process.raw(), raw);
        process
    }

    #[test]
    fn thread_lifecycle_tracks_join_state() {
        let mut table = ThreadTable::new();
        let thread = table.spawn(process(1)).unwrap();

        assert_eq!(table.join(thread), Ok(None));
        table.exit(thread, 7).unwrap();
        assert_eq!(table.join(thread), Ok(Some(7)));
    }

    #[test]
    fn sleeping_threads_wake_when_deadline_passes() {
        let mut table = ThreadTable::new();
        let thread = table.spawn(process(1)).unwrap();
        table.set_sleeping(thread, 50).unwrap();

        assert_eq!(table.wake_due(49), 0);
        assert_eq!(
            table.record(thread).unwrap().state(),
            ThreadState::Sleeping { wake_at_nanos: 50 }
        );
        assert_eq!(table.wake_due(50), 1);
        assert_eq!(table.record(thread).unwrap().state(), ThreadState::Runnable);
    }

    #[test]
    fn process_parallelism_counts_live_threads() {
        let mut table = ThreadTable::new();
        let first = table.spawn(process(1)).unwrap();
        let second = table.spawn(process(1)).unwrap();
        table.spawn(process(1)).unwrap();

        assert_eq!(table.process_parallelism(process(1)), 3);
        table.exit(first, 0).unwrap();
        assert_eq!(table.process_parallelism(process(1)), 2);
        table.signal(second, 9).unwrap();
        assert_eq!(table.process_parallelism(process(1)), 2);
    }
}

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;

use async_task::{Builder, Runnable, Task};
use spinning_top::Spinlock;

type ReadyQueue = Spinlock<VecDeque<Runnable>>;
pub type JoinHandle<T> = Task<T>;

#[derive(Clone)]
pub struct Spawner {
    ready_queue: Arc<ReadyQueue>,
}

pub struct Executor {
    ready_queue: Arc<ReadyQueue>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            ready_queue: Arc::new(Spinlock::new(VecDeque::new())),
        }
    }

    pub fn spawner(&self) -> Spawner {
        Spawner {
            ready_queue: self.ready_queue.clone(),
        }
    }

    pub fn run_until_stalled(&self) -> usize {
        let mut runnable_count = 0;

        loop {
            let Some(runnable) = critical_section::with(|_| self.ready_queue.lock().pop_front())
            else {
                return runnable_count;
            };

            runnable.run();
            runnable_count += 1;
        }
    }
}

impl Spawner {
    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let ready_queue = self.ready_queue.clone();
        let schedule = move |runnable| {
            critical_section::with(|_| {
                ready_queue.lock().push_back(runnable);
            });
        };
        let (runnable, task) = Builder::new().spawn(move |_| future, schedule);
        runnable.schedule();
        task
    }

    pub fn spawn_detached<Fut>(&self, future: Fut)
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawn(future).detach();
    }
}

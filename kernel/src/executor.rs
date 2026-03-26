extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use async_task::{Builder, Runnable, Task};
use spinning_top::Spinlock;

type ReadyQueue = Spinlock<VecDeque<Runnable>>;
pub type JoinHandle<T> = Task<T>;

/// Join handle for a task that is constrained to the spawning hart.
///
/// The marker makes the handle `!Send` and `!Sync`, which prevents accidental
/// migration of a local task to a different hart through the type system.
#[must_use = "tasks get canceled when dropped, use `.detach()` to run them in the background"]
pub struct LocalJoinHandle<T> {
    task: Task<T>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

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

    pub fn spawn_local<Fut>(&self, future: Fut) -> LocalJoinHandle<Fut::Output>
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        let ready_queue = self.ready_queue.clone();
        let schedule = move |runnable| {
            critical_section::with(|_| {
                ready_queue.lock().push_back(runnable);
            });
        };

        // SAFETY: the runnable is enqueued only into this executor's hart-local ready
        // queue. At the current stage there is no cross-hart injection path, so the
        // task will only be polled and dropped by the hart that spawned it.
        let (runnable, task) = unsafe { Builder::new().spawn_unchecked(move |_| future, schedule) };
        runnable.schedule();
        LocalJoinHandle {
            task,
            _not_send_or_sync: PhantomData,
        }
    }

    pub fn spawn_local_detached<Fut>(&self, future: Fut)
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        self.spawn_local(future).detach();
    }
}

impl<T> LocalJoinHandle<T> {
    pub fn detach(self) {
        self.task.detach();
    }

    pub async fn cancel(self) -> Option<T> {
        self.task.cancel().await
    }
}

impl<T> Future for LocalJoinHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

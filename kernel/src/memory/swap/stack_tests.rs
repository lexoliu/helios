//! The return stack: every handle pushed comes back to the owner once,
//! however many processors push at once.

use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU32, Ordering};

use super::stack::{Chain, Links, ReturnStack};

/// Links in an array, indexed by handle.
struct ArrayLinks(Vec<AtomicU32>);

impl ArrayLinks {
    fn new(handles: usize) -> Self {
        Self((0..=handles).map(|_| AtomicU32::new(0)).collect())
    }
}

impl Links for ArrayLinks {
    fn next_of(&self, handle: NonZeroU32) -> Option<NonZeroU32> {
        NonZeroU32::new(self.0[handle.get() as usize].load(Ordering::Acquire))
    }

    fn set_next(&self, handle: NonZeroU32, next: Option<NonZeroU32>) {
        self.0[handle.get() as usize].store(next.map_or(0, NonZeroU32::get), Ordering::Release);
    }
}

fn drain(chain: &mut Chain, links: &ArrayLinks) -> Vec<u32> {
    let mut handles = Vec::new();
    while let Some(handle) = chain.pop(links) {
        handles.push(handle.get());
    }
    handles
}

#[test]
fn the_most_recent_push_comes_back_first_and_an_empty_stack_yields_nothing() {
    let links = ArrayLinks::new(3);
    let stack = ReturnStack::new();
    assert!(stack.take_all().is_empty());
    for handle in 1..=3 {
        stack.push(&links, NonZeroU32::new(handle).unwrap());
    }
    let mut chain = stack.take_all();
    assert_eq!(drain(&mut chain, &links), [3, 2, 1]);
    assert!(stack.take_all().is_empty(), "taking empties the stack");
}

#[test]
fn handles_pushed_from_many_threads_all_come_back_once() {
    const THREADS: u32 = 4;
    const PER_THREAD: u32 = 5_000;
    let links = ArrayLinks::new((THREADS * PER_THREAD) as usize);
    let stack = ReturnStack::new();

    let mut taken = Vec::new();
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let (stack, links) = (&stack, &links);
            scope.spawn(move || {
                for offset in 0..PER_THREAD {
                    let handle = 1 + thread * PER_THREAD + offset;
                    stack.push(links, NonZeroU32::new(handle).unwrap());
                }
            });
        }
        // The owner takes while the pushers run.
        while taken.len() < (THREADS * PER_THREAD) as usize {
            let mut chain = stack.take_all();
            taken.extend(drain(&mut chain, &links));
        }
    });

    taken.sort_unstable();
    let expected: Vec<u32> = (1..=THREADS * PER_THREAD).collect();
    assert_eq!(taken, expected, "every handle exactly once");
}

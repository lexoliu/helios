//! Async channels for guest tasks.
//!
//! This is a direct re-export of the upstream `async-channel` crate. Channels
//! do not need kernel mediation, so Helios should reuse the ecosystem's MPMC
//! implementation instead of inventing another one.

pub use async_channel::{
    Receiver, Recv, RecvError, Send, SendError, Sender, TryRecvError, TrySendError, bounded,
    unbounded,
};

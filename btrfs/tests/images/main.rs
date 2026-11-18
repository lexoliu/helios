//! Mounts generated btrfs images and reads them back, intact and damaged.

mod corrupt;
mod extents;
mod fixture;
mod mkfs;
mod read;
mod walk;

#[cfg(feature = "write")]
mod log_replay;
#[cfg(feature = "write")]
mod writes;

//! The proof a filesystem operation demands of a descriptor.
//!
//! A descriptor's rights are plain flags, and a flag nobody tests is a right
//! nobody enforces. The operations in `fs.rs` therefore take a [`Granted`]
//! instead of the descriptor: a value that only [`FsDescriptor::grant`] and
//! [`FsDescriptor::grant_touch`] can make, and that they make only after
//! testing the kind and the right it names. A surface that forwards a call
//! has to mint the proof to make the call, and an operation cannot skip the
//! test because it never sees bare flags.
//!
//! Minting borrows the descriptor and touches no filesystem, so a refusal
//! always precedes any filesystem traffic, a host share's included.

use core::marker::PhantomData;

use super::*;

mod sealed {
    pub trait Sealed {}
}

/// A right an operation needs from a descriptor.
pub(crate) trait Right: sealed::Sealed {
    /// Holding any one of these grants the right.
    const ANY_OF: &'static [fs_types::DescriptorFlags];
    /// What the operation answers a descriptor without it.
    const REFUSED: fs_types::ErrorCode;
}

/// What the descriptor must name.
pub(crate) trait Kind: sealed::Sealed {
    /// The one node kind it must be, or `None` for any.
    const KIND: Option<FsNodeKind>;
    /// What the operation answers a descriptor of another kind.
    const MISMATCH: fs_types::ErrorCode;
}

/// The right to read a file's data or list a directory.
pub(crate) enum Read {}
/// The right to change a file's data.
pub(crate) enum Write {}
/// The right to change a directory's entries.
pub(crate) enum Mutate {}
/// What changing the times of an object below a directory needs: its
/// `write` or its `mutate-directory`.
pub(crate) enum Retime {}
/// What changing the times of a descriptor's own object needs, which depends
/// on its kind: see [`FsDescriptor::grant_touch`].
pub(crate) enum Touch {}

/// A descriptor that names a directory.
pub(crate) enum Directory {}
/// A descriptor that names a regular file.
pub(crate) enum RegularFile {}
/// A descriptor of any kind.
pub(crate) enum AnyKind {}

impl sealed::Sealed for Read {}
impl sealed::Sealed for Write {}
impl sealed::Sealed for Mutate {}
impl sealed::Sealed for Retime {}
impl sealed::Sealed for Directory {}
impl sealed::Sealed for RegularFile {}
impl sealed::Sealed for AnyKind {}

impl Right for Read {
    const ANY_OF: &'static [fs_types::DescriptorFlags] = &[fs_types::DescriptorFlags::READ];
    const REFUSED: fs_types::ErrorCode = fs_types::ErrorCode::NotPermitted;
}

impl Right for Write {
    const ANY_OF: &'static [fs_types::DescriptorFlags] = &[fs_types::DescriptorFlags::WRITE];
    const REFUSED: fs_types::ErrorCode = fs_types::ErrorCode::ReadOnly;
}

impl Right for Mutate {
    const ANY_OF: &'static [fs_types::DescriptorFlags] =
        &[fs_types::DescriptorFlags::MUTATE_DIRECTORY];
    const REFUSED: fs_types::ErrorCode = fs_types::ErrorCode::ReadOnly;
}

impl Right for Retime {
    const ANY_OF: &'static [fs_types::DescriptorFlags] = &[
        fs_types::DescriptorFlags::WRITE,
        fs_types::DescriptorFlags::MUTATE_DIRECTORY,
    ];
    const REFUSED: fs_types::ErrorCode = fs_types::ErrorCode::ReadOnly;
}

impl Kind for AnyKind {
    const KIND: Option<FsNodeKind> = None;
    const MISMATCH: fs_types::ErrorCode = fs_types::ErrorCode::Invalid;
}

impl Kind for Directory {
    const KIND: Option<FsNodeKind> = Some(FsNodeKind::Directory);
    const MISMATCH: fs_types::ErrorCode = fs_types::ErrorCode::NotDirectory;
}

impl Kind for RegularFile {
    const KIND: Option<FsNodeKind> = Some(FsNodeKind::File);
    const MISMATCH: fs_types::ErrorCode = fs_types::ErrorCode::IsDirectory;
}

/// Proof that a descriptor names a `K` and holds the right `R`.
///
/// Only [`FsDescriptor::grant`] and [`FsDescriptor::grant_touch`] make one:
/// its field is private to this module. It borrows the descriptor the call
/// cloned at entry, so a later `fd_fdstat_set_rights` narrows the table
/// entry and applies from the next call.
pub(crate) struct Granted<'d, K, R> {
    descriptor: &'d FsDescriptor,
    _proof: PhantomData<fn() -> (K, R)>,
}

impl<K, R> Clone for Granted<'_, K, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, R> Copy for Granted<'_, K, R> {}

impl<'d, K, R> Granted<'d, K, R> {
    fn new(descriptor: &'d FsDescriptor) -> Self {
        Self {
            descriptor,
            _proof: PhantomData,
        }
    }

    /// The descriptor the proof was made for.
    pub(crate) fn descriptor(&self) -> &'d FsDescriptor {
        self.descriptor
    }
}

impl FsDescriptor {
    /// Proves this descriptor names a `K` and holds `R`: the kind is tested
    /// first, then the right, and each refusal is the one its marker names.
    pub(crate) fn grant<K: Kind, R: Right>(&self) -> FsOpResult<Granted<'_, K, R>> {
        if K::KIND.is_some_and(|kind| kind != self.kind) {
            return Err(K::MISMATCH);
        }
        if !R::ANY_OF.iter().any(|right| self.flags.contains(*right)) {
            return Err(R::REFUSED);
        }
        Ok(Granted::new(self))
    }

    /// Proves this descriptor may have its own times changed: a directory
    /// needs `mutate-directory`, every other object needs `write`. The
    /// refusal is `read-only`.
    pub(crate) fn grant_touch(&self) -> FsOpResult<Granted<'_, AnyKind, Touch>> {
        let needed = if self.kind == FsNodeKind::Directory {
            fs_types::DescriptorFlags::MUTATE_DIRECTORY
        } else {
            fs_types::DescriptorFlags::WRITE
        };
        if !self.flags.contains(needed) {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        Ok(Granted::new(self))
    }
}

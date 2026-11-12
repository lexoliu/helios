//! The hand-written half of the wire format: the sinks and the decoder the
//! generated walks run on, and the primitives they are built from.
//!
//! The wire is little-endian, every item is padded to four bytes, and arrays
//! and pointers carry a 64-bit length prefix. Mesa's `vn_protocol_driver_*.h`
//! define the same primitives in C; the parity tests compare both.

use core::ffi::{c_char, c_void};

use crate::protocol::{EXTENSIONS, MAX_EXTENSION_NUMBER};

const _: () = assert!(
    cfg!(target_endian = "little"),
    "the wire format is little-endian and scalars are copied as they lie in memory"
);

/// Why an encode could not complete. The first error of a walk is kept.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("the command needs {needed} bytes and the buffer holds {capacity}")]
    BufferTooSmall { needed: usize, capacity: usize },
    #[error("member {member} is not serializable but was set")]
    NotSerializable { member: &'static str },
    #[error("a string is not terminated within its declared size")]
    StringNotTerminated,
    #[error("a struct has sType {found}, expected {expected}")]
    StructureType { expected: i32, found: i32 },
    #[error("a union was encoded with a tag that selects no member")]
    UnionTag,
}

/// Why a decode could not complete. The first error of a walk is kept.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("the reply holds {available} more bytes and {needed} are needed")]
    Truncated { needed: usize, available: usize },
    #[error("the reply has an array of {found} elements where {expected} are expected")]
    ArraySize { expected: u64, found: u64 },
    #[error("member {member} is not serializable but the reply carries it")]
    NotSerializable { member: &'static str },
    #[error("the reply has a struct with sType {found}, expected {expected}")]
    StructureType { expected: i32, found: i32 },
    #[error("the reply answers command {found}, expected {expected}")]
    CommandType { expected: i32, found: i32 },
    #[error("the reply chains a struct with sType {s_type}, which the request did not chain")]
    UnexpectedChainedStructure { s_type: i32 },
    #[error("the reply carries a union tag that selects no member")]
    UnionTag,
    #[error("the reply stores an object id into a null dispatchable handle")]
    NullDispatchableHandle,
    #[error("a string has a declared size of zero")]
    EmptyString,
    #[error("the reply carries the size {size}, which a 32-bit guest cannot hold")]
    SizeTooLarge { size: u64 },
}

/// What the renderer side of the connection supports: the Vulkan API version
/// its `vk.xml` is at, and the extensions it implements by number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RendererProtocol {
    api_version: u32,
    extensions: [u32; EXTENSION_MASK_WORDS],
}

/// Words of the extension bit mask.
pub const EXTENSION_MASK_WORDS: usize = MAX_EXTENSION_NUMBER as usize / 32 + 1;

/// `VK_MAKE_API_VERSION`.
pub const fn make_api_version(variant: u32, major: u32, minor: u32, patch: u32) -> u32 {
    (variant << 29) | (major << 22) | (minor << 12) | patch
}

/// `VK_MAKE_API_VERSION(0, major, minor, 0)`.
pub const fn api_version(major: u32, minor: u32) -> u32 {
    make_api_version(0, major, minor, 0)
}

/// One extension of the protocol, as `vn_info_extension` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionInfo {
    pub name: &'static str,
    pub number: u32,
    pub spec_version: u32,
}

impl RendererProtocol {
    /// A renderer at `api_version` that supports none of the extensions.
    pub const fn without_extensions(api_version: u32) -> Self {
        Self {
            api_version,
            extensions: [0; EXTENSION_MASK_WORDS],
        }
    }

    /// A renderer at `api_version` that supports every extension of the
    /// protocol.
    pub fn with_all_extensions(api_version: u32) -> Self {
        let mut protocol = Self::without_extensions(api_version);
        for extension in EXTENSIONS {
            protocol.enable_extension(extension.number);
        }
        protocol
    }

    pub fn enable_extension(&mut self, number: u32) {
        self.extensions[number as usize / 32] |= 1 << (number % 32);
    }

    pub fn has_api_version(&self, version: u32) -> bool {
        self.api_version >= version
    }

    pub fn has_extension(&self, number: u32) -> bool {
        self.extensions
            .get(number as usize / 32)
            .is_some_and(|word| word & (1 << (number % 32)) != 0)
    }

    pub fn api_version(&self) -> u32 {
        self.api_version
    }

    pub fn extension_mask(&self) -> &[u32] {
        &self.extensions
    }
}

/// An object id carrier: the guest object a dispatchable handle points at.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectBase {
    pub id: u64,
}

/// The first two members of every chainable struct.
#[repr(C)]
#[expect(non_snake_case, reason = "the members are named as in vk.xml")]
pub struct BaseStructure {
    pub sType: i32,
    pub pNext: *const c_void,
}

/// Casts between `*const T` and `*mut T`, whichever the source is.
pub trait PtrExt<T> {
    fn into_const(self) -> *const T;
    fn into_mut(self) -> *mut T;
}

impl<T> PtrExt<T> for *const T {
    fn into_const(self) -> *const T {
        self
    }
    fn into_mut(self) -> *mut T {
        self.cast_mut()
    }
}

impl<T> PtrExt<T> for *mut T {
    fn into_const(self) -> *const T {
        self.cast_const()
    }
    fn into_mut(self) -> *mut T {
        self
    }
}

/// Where a walk writes: a buffer, or a counter that only measures.
///
/// Both run the same generated walk, so a size is exactly what an encode of
/// the same arguments writes.
pub trait Sink {
    fn protocol(&self) -> &RendererProtocol;

    /// Appends `padded` bytes: `len` bytes from `data`, then zeros.
    ///
    /// # Safety
    /// `data` is valid for reads of `len` bytes, and `len <= padded`.
    unsafe fn write(&mut self, padded: usize, data: *const u8, len: usize);

    /// Records the first error of the walk.
    fn fail(&mut self, error: EncodeError);
}

/// Encodes into a caller-provided buffer.
pub struct Encoder<'a> {
    buffer: &'a mut [u8],
    position: usize,
    protocol: &'a RendererProtocol,
    error: Option<EncodeError>,
}

impl<'a> Encoder<'a> {
    pub fn new(buffer: &'a mut [u8], protocol: &'a RendererProtocol) -> Self {
        Self {
            buffer,
            position: 0,
            protocol,
            error: None,
        }
    }

    /// The bytes written, or the first error.
    pub fn finish(self) -> Result<usize, EncodeError> {
        match self.error {
            Some(error) => Err(error),
            None if self.position > self.buffer.len() => Err(EncodeError::BufferTooSmall {
                needed: self.position,
                capacity: self.buffer.len(),
            }),
            None => Ok(self.position),
        }
    }
}

impl Sink for Encoder<'_> {
    fn protocol(&self) -> &RendererProtocol {
        self.protocol
    }

    unsafe fn write(&mut self, padded: usize, data: *const u8, len: usize) {
        let end = self.position + padded;
        if end <= self.buffer.len() {
            // SAFETY: the caller guarantees `data` and `len`; the destination
            // range was just checked against the buffer.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data,
                    self.buffer.as_mut_ptr().add(self.position),
                    len,
                );
            }
            self.buffer[self.position + len..end].fill(0);
        }
        // Past the end only the position advances, so `finish` can say how
        // much space the command needs.
        self.position = end;
    }

    fn fail(&mut self, error: EncodeError) {
        self.error.get_or_insert(error);
    }
}

/// Counts what an encode would write.
pub struct Sizer<'a> {
    size: usize,
    protocol: &'a RendererProtocol,
    error: Option<EncodeError>,
}

impl<'a> Sizer<'a> {
    pub fn new(protocol: &'a RendererProtocol) -> Self {
        Self {
            size: 0,
            protocol,
            error: None,
        }
    }

    pub fn finish(self) -> Result<usize, EncodeError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.size),
        }
    }
}

impl Sink for Sizer<'_> {
    fn protocol(&self) -> &RendererProtocol {
        self.protocol
    }

    unsafe fn write(&mut self, padded: usize, _data: *const u8, _len: usize) {
        self.size += padded;
    }

    fn fail(&mut self, error: EncodeError) {
        self.error.get_or_insert(error);
    }
}

/// Decodes a reply from a byte slice.
pub struct Decoder<'a> {
    buffer: &'a [u8],
    position: usize,
    error: Option<DecodeError>,
}

impl<'a> Decoder<'a> {
    pub fn new(buffer: &'a [u8]) -> Self {
        Self {
            buffer,
            position: 0,
            error: None,
        }
    }

    /// The bytes consumed, or the first error.
    pub fn finish(self) -> Result<usize, DecodeError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.position),
        }
    }

    pub fn fail(&mut self, error: DecodeError) {
        self.error.get_or_insert(error);
    }

    /// Consumes `padded` bytes, copying the first `len` to `dst`.
    ///
    /// # Safety
    /// `dst` is valid for writes of `len` bytes, and `len <= padded`.
    unsafe fn read(&mut self, padded: usize, dst: *mut u8, len: usize) {
        let available = self.buffer.len() - self.position;
        if padded > available {
            self.fail(DecodeError::Truncated {
                needed: padded,
                available,
            });
            // SAFETY: `dst` is valid for `len` bytes by the caller.
            unsafe { core::ptr::write_bytes(dst, 0, len) };
            return;
        }
        // SAFETY: the source range was just checked; `dst` by the caller.
        unsafe {
            core::ptr::copy_nonoverlapping(self.buffer.as_ptr().add(self.position), dst, len);
        }
        self.position += padded;
    }

    fn peek_u64(&self) -> u64 {
        match self.buffer.get(self.position..self.position + 8) {
            Some(bytes) => u64::from_le_bytes(bytes.try_into().expect("an 8-byte range")),
            None => 0,
        }
    }
}

const fn padded(size: usize) -> usize {
    size.div_ceil(4) * 4
}

/// A type with a wire encoding: the primitives, and the enums over them.
pub trait Scalar: Copy {
    /// # Safety
    /// `value` points to an initialized `Self`.
    unsafe fn encode<S: Sink>(sink: &mut S, value: *const Self);

    /// # Safety
    /// `value` points to `Self` storage.
    unsafe fn decode(decoder: &mut Decoder<'_>, value: *mut Self);

    /// # Safety
    /// `values` points to `count` initialized `Self`.
    unsafe fn encode_array<S: Sink>(sink: &mut S, values: *const Self, count: usize);

    /// # Safety
    /// `values` points to storage for `count` `Self`.
    unsafe fn decode_array(decoder: &mut Decoder<'_>, values: *mut Self, count: usize);
}

macro_rules! scalar_primitive {
    ($($ty:ty),*) => {$(
        impl Scalar for $ty {
            unsafe fn encode<S: Sink>(sink: &mut S, value: *const Self) {
                // SAFETY: by the contract of `encode`.
                unsafe { sink.write(padded(size_of::<Self>()), value.cast(), size_of::<Self>()) }
            }
            unsafe fn decode(decoder: &mut Decoder<'_>, value: *mut Self) {
                // SAFETY: by the contract of `decode`.
                unsafe { decoder.read(padded(size_of::<Self>()), value.cast(), size_of::<Self>()) }
            }
            unsafe fn encode_array<S: Sink>(sink: &mut S, values: *const Self, count: usize) {
                let size = size_of::<Self>() * count;
                // SAFETY: by the contract of `encode_array`.
                unsafe { sink.write(padded(size), values.cast(), size) }
            }
            unsafe fn decode_array(decoder: &mut Decoder<'_>, values: *mut Self, count: usize) {
                let size = size_of::<Self>() * count;
                // SAFETY: by the contract of `decode_array`.
                unsafe { decoder.read(padded(size), values.cast(), size) }
            }
        }
    )*};
}

scalar_primitive!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

/// `size_t` is 64 bits on the wire whatever the guest's pointer width is.
impl Scalar for usize {
    unsafe fn encode<S: Sink>(sink: &mut S, value: *const Self) {
        // SAFETY: by the contract of `encode`.
        let wide = unsafe { *value } as u64;
        // SAFETY: `wide` is a local `u64`.
        unsafe { sink.write(8, (&raw const wide).cast(), 8) }
    }

    unsafe fn decode(decoder: &mut Decoder<'_>, value: *mut Self) {
        let mut wide = 0u64;
        // SAFETY: `wide` is a local `u64`.
        unsafe { decoder.read(8, (&raw mut wide).cast(), 8) };
        match usize::try_from(wide) {
            // SAFETY: `value` by the contract.
            Ok(size) => unsafe { *value = size },
            Err(_) => decoder.fail(DecodeError::SizeTooLarge { size: wide }),
        }
    }

    #[cfg(target_pointer_width = "64")]
    unsafe fn encode_array<S: Sink>(sink: &mut S, values: *const Self, count: usize) {
        // SAFETY: `usize` and `u64` have the same representation here.
        unsafe { u64::encode_array(sink, values.cast(), count) }
    }

    #[cfg(target_pointer_width = "32")]
    unsafe fn encode_array<S: Sink>(sink: &mut S, values: *const Self, count: usize) {
        // SAFETY: `usize` and `u32` have the same representation here.
        unsafe { encode_widened_array(sink, values.cast::<u32>(), count) }
    }

    #[cfg(target_pointer_width = "64")]
    unsafe fn decode_array(decoder: &mut Decoder<'_>, values: *mut Self, count: usize) {
        // SAFETY: `usize` and `u64` have the same representation here.
        unsafe { u64::decode_array(decoder, values.cast(), count) }
    }

    #[cfg(target_pointer_width = "32")]
    unsafe fn decode_array(decoder: &mut Decoder<'_>, values: *mut Self, count: usize) {
        // SAFETY: `usize` and `u32` have the same representation here.
        unsafe { decode_widened_array(decoder, values.cast::<u32>(), count) }
    }
}

/// Encodes `count` 32-bit values as 64-bit ones, each as `size_t` is on the
/// wire of a 32-bit guest. Public so it is tested on every host.
///
/// # Safety
/// `values` points to `count` initialized `u32`.
pub unsafe fn encode_widened_array<S: Sink>(sink: &mut S, values: *const u32, count: usize) {
    for index in 0..count {
        // SAFETY: by the contract.
        let wide = u64::from(unsafe { *values.add(index) });
        // SAFETY: `wide` is a local `u64`.
        unsafe { sink.write(8, (&raw const wide).cast(), 8) }
    }
}

/// The inverse of [`encode_widened_array`]; a value above `u32::MAX` fails
/// the decode, since the guest cannot hold it.
///
/// # Safety
/// `values` points to storage for `count` `u32`.
pub unsafe fn decode_widened_array(decoder: &mut Decoder<'_>, values: *mut u32, count: usize) {
    for index in 0..count {
        let mut wide = 0u64;
        // SAFETY: `wide` is a local `u64`.
        unsafe { decoder.read(8, (&raw mut wide).cast(), 8) };
        match u32::try_from(wide) {
            // SAFETY: by the contract.
            Ok(narrow) => unsafe { *values.add(index) = narrow },
            Err(_) => decoder.fail(DecodeError::SizeTooLarge { size: wide }),
        }
    }
}

/// `Scalar` for a `#[repr(transparent)]` newtype over a scalar.
macro_rules! scalar_newtype {
    ($name:ident, $repr:ty) => {
        impl $crate::runtime::Scalar for $name {
            unsafe fn encode<S: $crate::runtime::Sink>(sink: &mut S, value: *const Self) {
                // SAFETY: the newtype is transparent over `$repr`.
                unsafe { <$repr as $crate::runtime::Scalar>::encode(sink, value.cast()) }
            }
            unsafe fn decode(decoder: &mut $crate::runtime::Decoder<'_>, value: *mut Self) {
                // SAFETY: the newtype is transparent over `$repr`.
                unsafe { <$repr as $crate::runtime::Scalar>::decode(decoder, value.cast()) }
            }
            unsafe fn encode_array<S: $crate::runtime::Sink>(
                sink: &mut S,
                values: *const Self,
                count: usize,
            ) {
                // SAFETY: the newtype is transparent over `$repr`.
                unsafe {
                    <$repr as $crate::runtime::Scalar>::encode_array(sink, values.cast(), count)
                }
            }
            unsafe fn decode_array(
                decoder: &mut $crate::runtime::Decoder<'_>,
                values: *mut Self,
                count: usize,
            ) {
                // SAFETY: the newtype is transparent over `$repr`.
                unsafe {
                    <$repr as $crate::runtime::Scalar>::decode_array(decoder, values.cast(), count)
                }
            }
        }
    };
}
pub(crate) use scalar_newtype;

/// # Safety
/// `value` points to an initialized `T`.
pub unsafe fn encode_scalar<S: Sink, T: Scalar>(sink: &mut S, value: *const T) {
    // SAFETY: by the contract.
    unsafe { T::encode(sink, value) }
}

/// # Safety
/// `value` points to `T` storage.
pub unsafe fn decode_scalar<T: Scalar>(decoder: &mut Decoder<'_>, value: *mut T) {
    // SAFETY: by the contract.
    unsafe { T::decode(decoder, value) }
}

/// # Safety
/// `values` points to `count` initialized `T`.
pub unsafe fn encode_array<S: Sink, T: Scalar>(sink: &mut S, values: *const T, count: usize) {
    // SAFETY: by the contract.
    unsafe { T::encode_array(sink, values, count) }
}

/// # Safety
/// `values` points to storage for `count` `T`.
pub unsafe fn decode_array<T: Scalar>(decoder: &mut Decoder<'_>, values: *mut T, count: usize) {
    // SAFETY: by the contract.
    unsafe { T::decode_array(decoder, values, count) }
}

pub fn encode_array_size<S: Sink>(sink: &mut S, size: u64) {
    // SAFETY: `size` is a local `u64`.
    unsafe { sink.write(8, (&raw const size).cast(), 8) }
}

/// Reads an array size, which must be `expected`; a mismatch fails the decode
/// and reads as zero.
pub fn decode_array_size(decoder: &mut Decoder<'_>, expected: u64) -> u64 {
    let found = decode_array_size_unchecked(decoder);
    if found == expected {
        found
    } else {
        decoder.fail(DecodeError::ArraySize { expected, found });
        0
    }
}

pub fn decode_array_size_unchecked(decoder: &mut Decoder<'_>) -> u64 {
    let mut size = 0u64;
    // SAFETY: `size` is a local `u64`.
    unsafe { decoder.read(8, (&raw mut size).cast(), 8) };
    size
}

pub fn peek_array_size(decoder: &Decoder<'_>) -> u64 {
    decoder.peek_u64()
}

/// Writes whether `pointer` is set, and returns it.
pub fn encode_simple_pointer<S: Sink, T>(sink: &mut S, pointer: *const T) -> bool {
    encode_array_size(sink, u64::from(!pointer.is_null()));
    !pointer.is_null()
}

pub fn decode_simple_pointer(decoder: &mut Decoder<'_>) -> bool {
    decode_array_size_unchecked(decoder) != 0
}

/// # Safety
/// `data` points to `size` readable bytes.
pub unsafe fn encode_blob_array<S: Sink>(sink: &mut S, data: *const u8, size: usize) {
    // SAFETY: by the contract.
    unsafe { sink.write(padded(size), data, size) }
}

/// # Safety
/// `data` points to `size` writable bytes.
pub unsafe fn decode_blob_array(decoder: &mut Decoder<'_>, data: *mut u8, size: usize) {
    // SAFETY: by the contract.
    unsafe { decoder.read(padded(size), data, size) }
}

/// A string of `size` bytes, NUL included.
///
/// # Safety
/// `data` points to `size` readable bytes.
pub unsafe fn encode_char_array<S: Sink>(sink: &mut S, data: *const c_char, size: usize) {
    // SAFETY: by the contract.
    let terminated = unsafe { core::slice::from_raw_parts(data.cast::<u8>(), size) }.contains(&0);
    if !terminated {
        sink.fail(EncodeError::StringNotTerminated);
    }
    // SAFETY: by the contract.
    unsafe { encode_blob_array(sink, data.cast(), size) }
}

/// # Safety
/// `data` points to `size` writable bytes.
pub unsafe fn decode_char_array(decoder: &mut Decoder<'_>, data: *mut c_char, size: usize) {
    // SAFETY: by the contract.
    unsafe { decode_blob_array(decoder, data.cast(), size) };
    if size == 0 {
        decoder.fail(DecodeError::EmptyString);
    } else {
        // SAFETY: `size > 0` bytes are writable.
        unsafe { *data.add(size - 1) = 0 };
    }
}

/// `strlen(data) + 1`: the size of a string with its terminator.
///
/// # Safety
/// `data` points to a NUL-terminated string.
pub unsafe fn cstr_size(data: *const c_char) -> u64 {
    // SAFETY: by the contract.
    unsafe { core::ffi::CStr::from_ptr(data) }
        .to_bytes_with_nul()
        .len() as u64
}

/// A length read through `root`, which counts as zero while `root` is null.
pub fn len_if_set<T>(root: *const T, len: impl FnOnce() -> u64) -> u64 {
    if root.is_null() { 0 } else { len() }
}

/// The id a dispatchable handle carries; null is id 0.
///
/// # Safety
/// `object` is null or points to an `ObjectBase`.
pub unsafe fn handle_id(object: *mut ObjectBase) -> u64 {
    if object.is_null() {
        0
    } else {
        // SAFETY: non-null, by the contract.
        unsafe { (*object).id }
    }
}

/// Stores the id a reply carries into the object a dispatchable handle
/// points at.
///
/// # Safety
/// `object` is null or points to an `ObjectBase`.
pub unsafe fn store_handle_id(decoder: &mut Decoder<'_>, object: *mut ObjectBase, id: u64) {
    if object.is_null() {
        decoder.fail(DecodeError::NullDispatchableHandle);
    } else {
        // SAFETY: non-null, by the contract.
        unsafe { (*object).id = id };
    }
}

//! Byte-exact expectations written out by hand from the wire format, which
//! hold on every host. They pin what the Mesa parity test compares at scale
//! and run on hosts where Mesa's generator is not available.

use core::ffi::c_char;
use core::ptr::{null, null_mut};

use helios_venus::protocol::*;
use helios_venus::{
    DecodeError, Decoder, EncodeError, Encoder, RendererProtocol, Sizer, api_version,
    decode_widened_array, encode_widened_array,
};

fn protocol() -> RendererProtocol {
    RendererProtocol::with_all_extensions(api_version(1, 3))
}

fn u32le(value: u32) -> [u8; 4] {
    value.to_le_bytes()
}

fn u64le(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

#[test]
fn enumerate_instance_version_request() {
    let protocol = protocol();
    let mut version = 0u32;
    let mut buffer = [0xAAu8; 32];
    let mut encoder = Encoder::new(&mut buffer, &protocol);
    // SAFETY: `version` is live.
    unsafe { vn_encode_vkEnumerateInstanceVersion(&mut encoder, 0, &mut version) };
    let written = encoder.finish().unwrap();

    let mut expected = Vec::new();
    expected.extend(u32le(
        VK_COMMAND_TYPE_vkEnumerateInstanceVersion_EXT.0 as u32,
    ));
    expected.extend(u32le(0)); // command flags
    expected.extend(u64le(1)); // the output pointer is set
    assert_eq!(&buffer[..written], expected);
    // The padding after the data is zero, not what the buffer held.
    assert_eq!(written, 16);

    // SAFETY: `version` is live.
    let size = unsafe { vn_sizeof_vkEnumerateInstanceVersion(&protocol, &mut version) };
    assert_eq!(size, Ok(16));
}

#[test]
fn enumerate_instance_version_reply() {
    let mut reply = Vec::new();
    reply.extend(u32le(
        VK_COMMAND_TYPE_vkEnumerateInstanceVersion_EXT.0 as u32,
    ));
    reply.extend(u32le(VK_SUCCESS.0 as u32));
    reply.extend(u64le(1));
    reply.extend(u32le(api_version(1, 3)));

    let mut version = 0u32;
    let mut decoder = Decoder::new(&reply);
    // SAFETY: `version` is live.
    let result = unsafe { vn_decode_vkEnumerateInstanceVersion_reply(&mut decoder, &mut version) };
    assert_eq!(decoder.finish(), Ok(reply.len()));
    assert_eq!(result, VK_SUCCESS);
    assert_eq!(version, api_version(1, 3));

    // And the size the reply is expected to have.
    let protocol = protocol();
    // SAFETY: `version` is live.
    let size = unsafe { vn_sizeof_vkEnumerateInstanceVersion_reply(&protocol, &mut version) };
    assert_eq!(size, Ok(reply.len()));
}

#[test]
fn a_short_reply_fails_instead_of_reading_past_it() {
    let reply = u32le(VK_COMMAND_TYPE_vkEnumerateInstanceVersion_EXT.0 as u32);
    let mut version = 7u32;
    let mut decoder = Decoder::new(&reply);
    // SAFETY: `version` is live.
    unsafe { vn_decode_vkEnumerateInstanceVersion_reply(&mut decoder, &mut version) };
    assert!(matches!(
        decoder.finish(),
        Err(DecodeError::Truncated { .. })
    ));
}

#[test]
fn a_reply_to_another_command_fails() {
    let mut reply = Vec::new();
    reply.extend(u32le(0));
    reply.extend(u32le(0));
    reply.extend(u64le(0));
    let mut version = 0u32;
    let mut decoder = Decoder::new(&reply);
    // SAFETY: `version` is live.
    unsafe { vn_decode_vkEnumerateInstanceVersion_reply(&mut decoder, &mut version) };
    assert!(matches!(
        decoder.finish(),
        Err(DecodeError::CommandType { .. })
    ));
}

#[test]
fn a_buffer_that_is_too_small_reports_the_size_it_needs() {
    let protocol = protocol();
    let mut version = 0u32;
    let mut buffer = [0u8; 8];
    let mut encoder = Encoder::new(&mut buffer, &protocol);
    // SAFETY: `version` is live.
    unsafe { vn_encode_vkEnumerateInstanceVersion(&mut encoder, 0, &mut version) };
    assert_eq!(
        encoder.finish(),
        Err(EncodeError::BufferTooSmall {
            needed: 16,
            capacity: 8
        })
    );
}

fn application_info(name: *const c_char) -> VkApplicationInfo {
    VkApplicationInfo {
        sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        pNext: null(),
        pApplicationName: name,
        applicationVersion: 7,
        pEngineName: null(),
        engineVersion: 9,
        apiVersion: api_version(1, 3),
    }
}

/// A string member is its length with the terminator as a 64-bit prefix,
/// then the bytes padded to four; an unset one is a zero length.
#[test]
fn application_info_with_a_name() {
    let protocol = protocol();
    let info = application_info(c"hi".as_ptr());
    let mut buffer = [0u8; 128];
    let mut encoder = Encoder::new(&mut buffer, &protocol);
    // SAFETY: `info` is live and its string is terminated.
    unsafe { vn_encode_VkApplicationInfo(&mut encoder, &info) };
    let written = encoder.finish().unwrap();

    let mut expected = Vec::new();
    expected.extend(u32le(VK_STRUCTURE_TYPE_APPLICATION_INFO.0 as u32));
    expected.extend(u64le(0)); // no pNext
    expected.extend(u64le(3)); // "hi" and its terminator
    expected.extend(*b"hi\0\0");
    expected.extend(u32le(7)); // applicationVersion
    expected.extend(u64le(0)); // no engine name
    expected.extend(u32le(9)); // engineVersion
    expected.extend(u32le(api_version(1, 3)));
    assert_eq!(&buffer[..written], expected);

    // SAFETY: as above.
    let mut sizer = Sizer::new(&protocol);
    unsafe { vn_encode_VkApplicationInfo(&mut sizer, &info) };
    assert_eq!(sizer.finish(), Ok(expected.len()));
}

#[test]
fn an_unterminated_string_array_is_refused() {
    // A fixed-size char array that holds no terminator is a bug of the
    // caller's, caught instead of sent.
    let protocol = protocol();
    let mut properties = [0x41 as c_char; 256];
    let mut buffer = [0u8; 512];
    let mut encoder = Encoder::new(&mut buffer, &protocol);
    // SAFETY: the array is live and has the 256 bytes the wire asks for.
    unsafe {
        helios_venus::encode_char_array(&mut encoder, properties.as_mut_ptr(), properties.len());
    }
    assert_eq!(encoder.finish(), Err(EncodeError::StringNotTerminated));
}

#[test]
fn size_t_arrays_widen_for_a_guest_with_32_bit_pointers() {
    // What a wasm32 guest runs for `size_t` arrays, tested on whatever host
    // this is.
    let protocol = protocol();
    let values = [1u32, 2, 0xFFFF_FFFF];
    let mut buffer = [0u8; 64];
    let mut encoder = Encoder::new(&mut buffer, &protocol);
    // SAFETY: `values` is live.
    unsafe { encode_widened_array(&mut encoder, values.as_ptr(), values.len()) };
    let written = encoder.finish().unwrap();
    let mut expected = Vec::new();
    for value in values {
        expected.extend(u64le(u64::from(value)));
    }
    assert_eq!(&buffer[..written], expected);

    let mut decoded = [0u32; 3];
    let mut decoder = Decoder::new(&expected);
    // SAFETY: `decoded` is live.
    unsafe { decode_widened_array(&mut decoder, decoded.as_mut_ptr(), decoded.len()) };
    assert_eq!(decoder.finish(), Ok(expected.len()));
    assert_eq!(decoded, values);

    // A size the guest cannot hold is an error, not a truncation.
    let too_large = u64le(1 << 32);
    let mut decoder = Decoder::new(&too_large);
    let mut one = [0u32; 1];
    // SAFETY: `one` is live.
    unsafe { decode_widened_array(&mut decoder, one.as_mut_ptr(), 1) };
    assert_eq!(
        decoder.finish(),
        Err(DecodeError::SizeTooLarge { size: 1 << 32 })
    );
}

#[test]
fn handles_encode_as_their_object_ids() {
    let protocol = protocol();
    let mut object = helios_venus::ObjectBase {
        id: 0x1122_3344_5566_7788,
    };
    let instance = VkInstance(&mut object);
    let buffer_handle = VkBuffer(42);
    let null_instance = VkInstance(null_mut());

    let mut out = [0u8; 32];
    let mut encoder = Encoder::new(&mut out, &protocol);
    // SAFETY: the handles are live.
    unsafe {
        vn_encode_VkInstance(&mut encoder, &instance);
        vn_encode_VkBuffer(&mut encoder, &buffer_handle);
        vn_encode_VkInstance(&mut encoder, &null_instance);
    }
    let written = encoder.finish().unwrap();
    let mut expected = Vec::new();
    expected.extend(u64le(0x1122_3344_5566_7788));
    expected.extend(u64le(42));
    expected.extend(u64le(0));
    assert_eq!(&out[..written], expected);
}

/// Enumerators an extension defines by offset: `1000000000 + (number - 1) *
/// 1000 + offset`, negative for `dir="-"`.
#[test]
fn offset_enumerators_have_the_values_of_the_registry() {
    assert_eq!(VK_STRUCTURE_TYPE_RENDERING_AREA_INFO.0, 1_000_470_003);
    assert_eq!(VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO.0, 12);
    assert_eq!(VK_ERROR_OUT_OF_POOL_MEMORY.0, -1_000_069_000);
    assert_eq!(VK_ERROR_OUT_OF_HOST_MEMORY.0, -1);
}

#[test]
fn the_extension_table_is_sorted_for_binary_search() {
    let names: Vec<&str> = EXTENSIONS.iter().map(|extension| extension.name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    assert!(EXTENSIONS.iter().all(|e| e.number <= MAX_EXTENSION_NUMBER));
}

/// The layout the generated types must have on a 32-bit guest are asserted at
/// compile time in the generated module for both pointer widths; this pins
/// the widths those assertions are made for.
#[test]
fn pointer_sized_members_follow_the_target() {
    assert_eq!(size_of::<VkInstance>(), size_of::<usize>());
    assert_eq!(size_of::<VkBuffer>(), 8);
    assert_eq!(size_of::<VkDeviceSize>(), 8);
    // A struct with a pointer, a 64-bit integer and a 32-bit one: the
    // generated assertions check its size and offsets for 4- and 8-byte
    // pointers, and this checks the one that was built.
    let expected = if size_of::<usize>() == 8 { 56 } else { 40 };
    assert_eq!(size_of::<VkBufferCreateInfo>(), expected);
}

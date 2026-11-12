//! The generated encoders against Mesa's own generator.
//!
//! The pinned `vn_protocol.py` generates the driver-side encoders and the
//! renderer-side reply encoders. For every command both generators emit, the
//! same arguments (C memory of the host's layout, built from the registry by
//! `parity::fill`) go through Mesa's C code and through the generated Rust
//! code, and the bytes must be equal; replies Mesa encodes must decode in
//! Rust to arguments that encode back to the same bytes.
//!
//! What this proves about a `wasm32` guest and what it does not: the C side
//! is compiled for the host (64-bit pointers), so it proves that the wire
//! logic (which members and extensions are sent, in which order, with which
//! sizes and padding) equals Mesa's, with the arguments laid out as the host
//! lays them out. It says nothing about the layout of the generated types on
//! a 32-bit target; that is closed independently of the host by the layout
//! assertions the generator emits for both pointer widths (checked when the
//! crate builds for `wasm32-wasip2`), by `venus-gen`'s tests of the layout
//! model against a C compiler targeting wasm32, and by the golden tests of
//! the `size_t` widening, which run on every host.

mod parity;
mod support;

#[expect(
    non_snake_case,
    reason = "the generated entry points are named after the registry's commands"
)]
mod generated {
    use helios_venus::protocol::*;
    use helios_venus::{Decoder, EncodeError, Encoder, RendererProtocol};

    include!(concat!(env!("OUT_DIR"), "/parity.rs"));
}

use std::ffi::c_void;

use helios_venus::protocol::MAX_EXTENSION_NUMBER;
use helios_venus::{Decoder, Encoder, RendererProtocol, api_version};
use helios_venus_gen::analysis::Analysis;
use helios_venus_gen::registry::Registry;
use parity::fill::{Config, Filler, PNext};
use parity::shim::Mesa;

/// `VK_COMMAND_GENERATE_REPLY_BIT_EXT`.
const GENERATE_REPLY: u32 = 1;
/// The return value the replies carry (truncated to the command's type).
const RETURN_VALUE: u64 = 0x8877_6655_4433_2211;

struct Scenario {
    name: &'static str,
    api: (u32, u32),
    all_extensions: bool,
    pnext: PNext,
    layout_seed: u64,
    /// Whether replies are compared. Mesa's renderer encodes replies for
    /// every extension, so only a protocol with every extension is
    /// comparable.
    replies: bool,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "every extension, full pNext chains",
        api: (1, 4),
        all_extensions: true,
        pnext: PNext::Chain,
        layout_seed: 0,
        replies: true,
    },
    Scenario {
        name: "every extension, other enumerants and flags",
        api: (1, 4),
        all_extensions: true,
        pnext: PNext::Chain,
        layout_seed: 1,
        replies: true,
    },
    Scenario {
        name: "every extension, null pNext",
        api: (1, 4),
        all_extensions: true,
        pnext: PNext::Null,
        layout_seed: 0,
        replies: true,
    },
    Scenario {
        name: "every extension, an unknown structure at the head of each chain",
        api: (1, 4),
        all_extensions: true,
        pnext: PNext::ChainWithUnknown,
        layout_seed: 0,
        replies: true,
    },
    Scenario {
        name: "no extension: pNext structs of extensions are dropped",
        api: (1, 4),
        all_extensions: false,
        pnext: PNext::Chain,
        layout_seed: 0,
        replies: false,
    },
    Scenario {
        name: "API 1.0 with every extension: structs of later core versions are dropped",
        api: (1, 0),
        all_extensions: true,
        pnext: PNext::Chain,
        layout_seed: 0,
        replies: false,
    },
];

/// The commands the bring-up path and a wgpu-hal Vulkan backend use; all of
/// them must be among the generated commands this test covers.
const REQUIRED: &[&str] = &[
    "vkEnumerateInstanceVersion",
    "vkEnumerateInstanceExtensionProperties",
    "vkEnumerateInstanceLayerProperties",
    "vkCreateInstance",
    "vkDestroyInstance",
    "vkEnumeratePhysicalDevices",
    "vkGetPhysicalDeviceProperties",
    "vkGetPhysicalDeviceProperties2",
    "vkGetPhysicalDeviceFeatures2",
    "vkGetPhysicalDeviceFormatProperties",
    "vkGetPhysicalDeviceImageFormatProperties2",
    "vkGetPhysicalDeviceMemoryProperties",
    "vkGetPhysicalDeviceQueueFamilyProperties",
    "vkEnumerateDeviceExtensionProperties",
    "vkCreateDevice",
    "vkDestroyDevice",
    "vkGetDeviceQueue",
    "vkQueueSubmit",
    "vkQueueWaitIdle",
    "vkDeviceWaitIdle",
    "vkAllocateMemory",
    "vkFreeMemory",
    "vkFlushMappedMemoryRanges",
    "vkInvalidateMappedMemoryRanges",
    "vkCreateBuffer",
    "vkDestroyBuffer",
    "vkGetBufferMemoryRequirements2",
    "vkBindBufferMemory",
    "vkCreateImage",
    "vkDestroyImage",
    "vkGetImageMemoryRequirements2",
    "vkBindImageMemory",
    "vkCreateImageView",
    "vkCreateSampler",
    "vkCreateShaderModule",
    "vkCreateDescriptorSetLayout",
    "vkCreateDescriptorPool",
    "vkAllocateDescriptorSets",
    "vkUpdateDescriptorSets",
    "vkCreatePipelineLayout",
    "vkCreateComputePipelines",
    "vkCreateGraphicsPipelines",
    "vkCreateRenderPass",
    "vkCreateFramebuffer",
    "vkCreateCommandPool",
    "vkAllocateCommandBuffers",
    "vkBeginCommandBuffer",
    "vkEndCommandBuffer",
    "vkResetCommandBuffer",
    "vkCmdBindPipeline",
    "vkCmdBindDescriptorSets",
    "vkCmdDispatch",
    "vkCmdCopyBuffer",
    "vkCmdCopyBufferToImage",
    "vkCmdCopyImageToBuffer",
    "vkCmdPipelineBarrier",
    "vkCmdBeginRenderPass",
    "vkCmdEndRenderPass",
    "vkCmdDraw",
    "vkCreateFence",
    "vkWaitForFences",
    "vkResetFences",
    "vkCreateSemaphore",
    "vkGetSemaphoreCounterValue",
    "vkWaitSemaphores",
    "vkCreateQueryPool",
];

struct Failures {
    list: Vec<String>,
}

impl Failures {
    fn add(&mut self, scenario: &Scenario, command: &str, what: String) {
        self.list
            .push(format!("{command} [{}]: {what}", scenario.name));
    }
}

/// Where two byte strings first differ.
fn first_difference(expected: &[u8], actual: &[u8]) -> Option<String> {
    if expected == actual {
        return None;
    }
    let at = expected
        .iter()
        .zip(actual)
        .position(|(a, b)| a != b)
        .unwrap_or(expected.len().min(actual.len()));
    let window = |bytes: &[u8]| {
        let from = at.saturating_sub(8);
        format!("{:02x?}", &bytes[from..bytes.len().min(at + 16)])
    };
    Some(format!(
        "Mesa and Rust differ at byte {at} (Mesa {} bytes, Rust {} bytes); Mesa ...{} Rust ...{}",
        expected.len(),
        actual.len(),
        window(expected),
        window(actual)
    ))
}

fn protocol(scenario: &Scenario) -> RendererProtocol {
    let api = api_version(scenario.api.0, scenario.api.1);
    if scenario.all_extensions {
        RendererProtocol::with_all_extensions(api)
    } else {
        RendererProtocol::without_extensions(api)
    }
}

/// The extension table of Mesa's generated `vn_protocol_driver_info.h` is
/// the table the generated Rust carries: names, numbers, spec versions, the
/// order, and the highest number.
fn check_extension_table(header: &std::path::Path) {
    let text = std::fs::read_to_string(header)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", header.display()));
    let mut mesa = Vec::new();
    for line in text.lines() {
        let Some(row) = line.trim().strip_prefix("{ \"") else {
            continue;
        };
        let row = row.trim_end_matches(" },").trim_end_matches("},");
        let mut parts = row.split("\", ");
        let name = parts.next().expect("an extension row has a name");
        let rest = parts.next().expect("an extension row has numbers");
        let (number, spec_version) = rest.split_once(", ").expect("a number and a spec version");
        mesa.push((
            name.to_owned(),
            number.trim().parse::<u32>().expect("an extension number"),
            spec_version.trim().parse::<u32>().expect("a spec version"),
        ));
    }
    let rust: Vec<(String, u32, u32)> = helios_venus::protocol::EXTENSIONS
        .iter()
        .map(|e| (e.name.to_owned(), e.number, e.spec_version))
        .collect();
    assert!(
        !mesa.is_empty(),
        "no extension rows were read from {}",
        header.display()
    );
    assert_eq!(rust, mesa, "the extension table");
    let max = text
        .lines()
        .find_map(|line| line.strip_prefix("#define VN_INFO_EXTENSION_MAX_NUMBER ("))
        .and_then(|rest| rest.strip_suffix(')'))
        .expect("the highest extension number")
        .parse::<u32>()
        .expect("a number");
    assert_eq!(MAX_EXTENSION_NUMBER, max, "the highest extension number");
}

#[test]
fn encoders_match_mesas_generator() {
    let inputs = support::inputs();
    let registry = Registry::parse(
        &inputs.path("xmls/vk.xml"),
        &[
            &inputs.path("xmls/VK_EXT_command_serialization.xml"),
            &inputs.path("xmls/VK_MESA_venus_protocol.xml"),
        ],
    )
    .expect("the registry parses");
    let an = Analysis::new(registry).expect("the registry is analysed");

    // The rows the two sides are zipped over.
    assert_eq!(
        generated::COMMANDS.len(),
        an.generated_commands.len(),
        "the support module covers every generated command"
    );
    for (entry, &id) in generated::COMMANDS.iter().zip(&an.generated_commands) {
        assert_eq!(entry.name, an.reg.types[id].name);
    }
    for required in REQUIRED {
        assert!(
            generated::COMMANDS.iter().any(|c| c.name == *required),
            "{required} is not a generated command"
        );
    }

    // Next to the pinned inputs, under the ignored build output.
    let work = inputs
        .root()
        .parent()
        .expect("the cache directory has a parent")
        .join("venus-parity");
    let mesa = Mesa::build(&inputs, &an, &work).unwrap_or_else(|error| panic!("{error}"));
    let drivers: Vec<_> = generated::COMMANDS
        .iter()
        .map(|c| {
            mesa.driver_command(c.name)
                .unwrap_or_else(|e| panic!("{e}"))
        })
        .collect();
    let renderers: Vec<_> = generated::COMMANDS
        .iter()
        .map(|c| {
            mesa.renderer_command(c.name)
                .unwrap_or_else(|e| panic!("{e}"))
        })
        .collect();

    check_extension_table(&work.join("driver/vn_protocol_driver_info.h"));

    let mut failures = Failures { list: Vec::new() };
    let mut compared = 0usize;
    let mut replies = 0usize;
    // Bytes compared per scenario, to show the scenarios differ in what they
    // put on the wire.
    let mut totals = Vec::new();
    for scenario in SCENARIOS {
        let mut total = 0usize;
        let protocol = protocol(scenario);
        let enabled: Vec<u8> = (0..=MAX_EXTENSION_NUMBER)
            .map(|n| u8::from(protocol.has_extension(n)))
            .collect();
        mesa.set_protocol(api_version(scenario.api.0, scenario.api.1), &enabled)
            .unwrap_or_else(|e| panic!("{e}"));

        for (index, entry) in generated::COMMANDS.iter().enumerate() {
            let id = an.generated_commands[index];
            let cfg = |value_seed| Config {
                pnext: scenario.pnext,
                layout_seed: scenario.layout_seed,
                value_seed,
            };
            let mut filler = Filler::new(&an, cfg(1)).unwrap_or_else(|e| panic!("{e}"));
            let storage = match filler.command_args(id) {
                Ok(storage) => storage,
                Err(error) => {
                    failures.add(
                        scenario,
                        entry.name,
                        format!("cannot fill the arguments: {error}"),
                    );
                    continue;
                }
            };
            let mut args: Vec<*mut c_void> = storage.iter().map(|p| p.cast()).collect();
            let driver = &drivers[index];

            // SAFETY: `args` points to the filled storage of every parameter,
            // which `filler` keeps alive.
            let mesa_size = unsafe { driver.sizeof(&mut args) };
            let mut mesa = vec![0u8; mesa_size];
            // SAFETY: as above.
            let (mesa_len, overflow) =
                unsafe { driver.encode(&mut mesa, GENERATE_REPLY, &mut args) };
            if overflow || mesa_len != mesa_size {
                failures.add(
                    scenario,
                    entry.name,
                    format!("Mesa's sizeof is {mesa_size} but its encoder wrote {mesa_len}"),
                );
                continue;
            }

            let command_type = u32::from_le_bytes(
                mesa[..4]
                    .try_into()
                    .expect("a command starts with its type"),
            );
            if command_type != entry.command_type {
                failures.add(
                    scenario,
                    entry.name,
                    format!(
                        "Mesa's command type is {command_type}, the constant is {}",
                        entry.command_type
                    ),
                );
            }

            // SAFETY: as above.
            let rust_size = unsafe { (entry.sizeof)(&protocol, &args) };
            if rust_size != Ok(mesa_size) {
                failures.add(
                    scenario,
                    entry.name,
                    format!("sizeof is {rust_size:?}, Mesa's is {mesa_size}"),
                );
            }
            total += mesa_size;
            let mut rust = vec![0u8; mesa_size];
            let mut encoder = Encoder::new(&mut rust, &protocol);
            // SAFETY: as above.
            unsafe { (entry.encode)(&mut encoder, GENERATE_REPLY, &args) };
            match encoder.finish() {
                Ok(written) if written == mesa_size => {
                    if let Some(difference) = first_difference(&mesa, &rust) {
                        failures.add(scenario, entry.name, format!("command: {difference}"));
                    }
                }
                other => failures.add(
                    scenario,
                    entry.name,
                    format!("the encoder finished with {other:?}, Mesa wrote {mesa_size} bytes"),
                ),
            }
            compared += 1;

            // SAFETY: as above.
            let mesa_reply_size = unsafe { driver.sizeof_reply(&mut args) };
            // SAFETY: as above.
            let rust_reply_size = unsafe { (entry.sizeof_reply)(&protocol, &args) };
            if rust_reply_size != Ok(mesa_reply_size) {
                failures.add(
                    scenario,
                    entry.name,
                    format!("reply sizeof is {rust_reply_size:?}, Mesa's is {mesa_reply_size}"),
                );
            }
            if !scenario.replies {
                continue;
            }

            let mask = match entry.ret_size {
                0 => 0,
                8 => u64::MAX,
                size => (1u64 << (size * 8)) - 1,
            };
            let ret = RETURN_VALUE & mask;
            let mut mesa_reply = vec![0u8; mesa_reply_size];
            // SAFETY: as above.
            let (len, overflow) =
                unsafe { renderers[index].encode_reply(&mut mesa_reply, ret, &mut args) };
            if overflow || len != mesa_reply_size {
                failures.add(
                    scenario,
                    entry.name,
                    format!("the driver's reply sizeof is {mesa_reply_size} but the renderer wrote {len}"),
                );
                continue;
            }
            let mut rust_reply = vec![0u8; mesa_reply_size];
            let mut encoder = Encoder::new(&mut rust_reply, &protocol);
            // SAFETY: as above.
            unsafe { (entry.encode_reply)(&mut encoder, ret.to_le_bytes(), &args) };
            match encoder.finish() {
                Ok(written) if written == mesa_reply_size => {
                    if let Some(difference) = first_difference(&mesa_reply, &rust_reply) {
                        failures.add(scenario, entry.name, format!("reply: {difference}"));
                    }
                }
                other => failures.add(
                    scenario,
                    entry.name,
                    format!("the reply encoder finished with {other:?}, Mesa wrote {mesa_reply_size} bytes"),
                ),
            }

            // Decode what Mesa wrote into arguments with other values; what
            // the decoder stored must encode back to the same bytes.
            let mut other = Filler::new(&an, cfg(2)).unwrap_or_else(|e| panic!("{e}"));
            let other_storage = other
                .command_args(id)
                .expect("the first fill of these arguments worked");
            let other_args: Vec<*mut c_void> = other_storage.iter().map(|p| p.cast()).collect();
            let mut decoder = Decoder::new(&mesa_reply);
            // SAFETY: `other_args` points to the filled storage of every parameter.
            let decoded = unsafe { (entry.decode_reply)(&mut decoder, &other_args) };
            match decoder.finish() {
                Ok(consumed) if consumed == mesa_reply.len() => {}
                outcome => failures.add(
                    scenario,
                    entry.name,
                    format!(
                        "the decoder finished with {outcome:?} over {} bytes",
                        mesa_reply.len()
                    ),
                ),
            }
            if decoded[..entry.ret_size] != ret.to_le_bytes()[..entry.ret_size] {
                failures.add(
                    scenario,
                    entry.name,
                    format!("the decoded return value is {decoded:02x?}, Mesa encoded {ret:#x}"),
                );
            }
            let mut again = vec![0u8; mesa_reply_size];
            let mut encoder = Encoder::new(&mut again, &protocol);
            // SAFETY: as above.
            unsafe { (entry.encode_reply)(&mut encoder, decoded, &other_args) };
            match encoder.finish() {
                Ok(_) => {
                    if let Some(difference) = first_difference(&mesa_reply, &again) {
                        failures.add(
                            scenario,
                            entry.name,
                            format!("the decoded reply encodes differently: {difference}"),
                        );
                    }
                }
                Err(error) => failures.add(
                    scenario,
                    entry.name,
                    format!("the decoded reply does not encode: {error:?}"),
                ),
            }
            replies += 1;
        }
        totals.push(total);
    }

    assert!(
        failures.list.is_empty(),
        "{} of {compared} command encodings and {replies} replies differ from Mesa's:\n{}",
        failures.list.len(),
        failures
            .list
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The scenarios are not all the same run: chains add bytes over a null
    // pNext, and dropping extensions or API versions removes some again.
    let [chain, _, null, unknown, no_extensions, api_1_0] = totals[..] else {
        panic!("one total per scenario")
    };
    assert!(
        chain > null,
        "full chains add structs over null pNext ({chain} vs {null})"
    );
    assert_eq!(
        chain, unknown,
        "an unknown structure in a chain is skipped, not encoded"
    );
    assert!(
        no_extensions < chain,
        "dropping extensions drops structs ({no_extensions} vs {chain})"
    );
    assert!(
        api_1_0 < chain,
        "dropping API versions drops structs ({api_1_0} vs {chain})"
    );
    eprintln!("compared {compared} command encodings and {replies} replies with Mesa's generator");
}

#[test]
fn a_difference_is_reported() {
    assert!(first_difference(&[1, 2, 3], &[1, 2, 3]).is_none());
    assert!(first_difference(&[1, 2, 3], &[1, 9, 3]).is_some_and(|d| d.contains("byte 1")));
    assert!(first_difference(&[1, 2, 3], &[1, 2]).is_some());
}

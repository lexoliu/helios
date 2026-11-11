//! The generator policy venus-protocol keeps as Python literals in
//! `vn_protocol.py` and `vkxml.py`, ported as data.
//!
//! The `policy_literals` test reads the pinned Python sources and fails when
//! any of these tables differs from the literal it was ported from, so a
//! protocol bump cannot drift silently. Rules the Python keeps in code rather
//! than in a literal (the in/out classification, the `VkBaseInStructure`
//! exclusion, the `ppBuildRangeInfos` length workaround) are ported in the
//! modules that apply them and are covered by the pinned file hashes and the
//! Mesa parity test.

/// `VN_WIRE_FORMAT_VERSION`.
pub const WIRE_FORMAT_VERSION: u32 = 1;

/// `VK_XML_EXTENSION_LIST`: only these extensions' types and commands are
/// generated.
pub const EXTENSION_LIST: &[&str] = &[
    // Venus extensions
    "VK_EXT_command_serialization",
    "VK_MESA_venus_protocol",
    // promoted to VK_VERSION_1_1
    "VK_KHR_16bit_storage",
    "VK_KHR_bind_memory2",
    "VK_KHR_dedicated_allocation",
    "VK_KHR_descriptor_update_template",
    "VK_KHR_device_group",
    "VK_KHR_device_group_creation",
    "VK_KHR_external_fence",
    "VK_KHR_external_fence_capabilities",
    "VK_KHR_external_memory",
    "VK_KHR_external_memory_capabilities",
    "VK_KHR_external_semaphore",
    "VK_KHR_external_semaphore_capabilities",
    "VK_KHR_get_memory_requirements2",
    "VK_KHR_get_physical_device_properties2",
    "VK_KHR_maintenance1",
    "VK_KHR_maintenance2",
    "VK_KHR_maintenance3",
    "VK_KHR_multiview",
    "VK_KHR_relaxed_block_layout",
    "VK_KHR_sampler_ycbcr_conversion",
    "VK_KHR_shader_draw_parameters",
    "VK_KHR_storage_buffer_storage_class",
    "VK_KHR_variable_pointers",
    // promoted to VK_VERSION_1_2
    "VK_KHR_8bit_storage",
    "VK_KHR_buffer_device_address",
    "VK_KHR_create_renderpass2",
    "VK_KHR_depth_stencil_resolve",
    "VK_KHR_draw_indirect_count",
    "VK_KHR_driver_properties",
    "VK_KHR_image_format_list",
    "VK_KHR_imageless_framebuffer",
    "VK_KHR_sampler_mirror_clamp_to_edge",
    "VK_KHR_separate_depth_stencil_layouts",
    "VK_KHR_shader_atomic_int64",
    "VK_KHR_shader_float16_int8",
    "VK_KHR_shader_float_controls",
    "VK_KHR_shader_subgroup_extended_types",
    "VK_KHR_spirv_1_4",
    "VK_KHR_timeline_semaphore",
    "VK_KHR_uniform_buffer_standard_layout",
    "VK_KHR_vulkan_memory_model",
    "VK_EXT_descriptor_indexing",
    "VK_EXT_host_query_reset",
    "VK_EXT_sampler_filter_minmax",
    "VK_EXT_scalar_block_layout",
    "VK_EXT_separate_stencil_usage",
    "VK_EXT_shader_viewport_index_layer",
    // promoted to VK_VERSION_1_3
    "VK_KHR_copy_commands2",
    "VK_KHR_dynamic_rendering",
    "VK_KHR_format_feature_flags2",
    "VK_KHR_maintenance4",
    "VK_KHR_shader_integer_dot_product",
    "VK_KHR_shader_non_semantic_info",
    "VK_KHR_shader_terminate_invocation",
    "VK_KHR_synchronization2",
    "VK_KHR_zero_initialize_workgroup_memory",
    "VK_EXT_4444_formats",
    "VK_EXT_extended_dynamic_state",
    "VK_EXT_extended_dynamic_state2",
    "VK_EXT_image_robustness",
    "VK_EXT_inline_uniform_block",
    "VK_EXT_pipeline_creation_cache_control",
    "VK_EXT_pipeline_creation_feedback",
    "VK_EXT_private_data",
    "VK_EXT_shader_demote_to_helper_invocation",
    "VK_EXT_subgroup_size_control",
    "VK_EXT_texel_buffer_alignment",
    "VK_EXT_texture_compression_astc_hdr",
    "VK_EXT_tooling_info",
    "VK_EXT_ycbcr_2plane_444_formats",
    // promoted to VK_VERSION_1_4
    "VK_KHR_dynamic_rendering_local_read",
    "VK_KHR_global_priority",
    "VK_KHR_index_type_uint8",
    "VK_KHR_line_rasterization",
    "VK_KHR_load_store_op_none",
    "VK_KHR_maintenance5",
    "VK_KHR_maintenance6",
    "VK_KHR_map_memory2",
    "VK_KHR_push_descriptor",
    "VK_KHR_shader_expect_assume",
    "VK_KHR_shader_float_controls2",
    "VK_KHR_shader_subgroup_rotate",
    "VK_KHR_vertex_attribute_divisor",
    "VK_EXT_host_image_copy",
    "VK_EXT_pipeline_protected_access",
    "VK_EXT_pipeline_robustness",
    // KHR extensions
    "VK_KHR_acceleration_structure",
    "VK_KHR_calibrated_timestamps",
    "VK_KHR_compute_shader_derivatives",
    "VK_KHR_cooperative_matrix",
    "VK_KHR_copy_memory_indirect",
    "VK_KHR_deferred_host_operations",
    "VK_KHR_depth_clamp_zero_one",
    "VK_KHR_external_fence_fd",
    "VK_KHR_external_memory_fd",
    "VK_KHR_external_semaphore_fd",
    "VK_KHR_fragment_shader_barycentric",
    "VK_KHR_fragment_shading_rate",
    "VK_KHR_maintenance7",
    "VK_KHR_maintenance8",
    "VK_KHR_maintenance9",
    "VK_KHR_maintenance10",
    "VK_KHR_maintenance11",
    "VK_KHR_pipeline_library",
    "VK_KHR_ray_query",
    "VK_KHR_ray_tracing_maintenance1",
    "VK_KHR_ray_tracing_pipeline",
    "VK_KHR_ray_tracing_position_fetch",
    "VK_KHR_robustness2",
    "VK_KHR_shader_bfloat16",
    "VK_KHR_shader_clock",
    "VK_KHR_shader_constant_data",
    "VK_KHR_shader_fma",
    "VK_KHR_shader_maximal_reconvergence",
    "VK_KHR_shader_quad_control",
    "VK_KHR_shader_relaxed_extended_instruction",
    "VK_KHR_shader_subgroup_uniform_control_flow",
    "VK_KHR_shader_untyped_pointers",
    "VK_KHR_unified_image_layouts",
    "VK_KHR_workgroup_memory_explicit_layout",
    // EXT extensions
    "VK_EXT_attachment_feedback_loop_dynamic_state",
    "VK_EXT_attachment_feedback_loop_layout",
    "VK_EXT_blend_operation_advanced",
    "VK_EXT_border_color_swizzle",
    "VK_EXT_buffer_device_address",
    "VK_EXT_calibrated_timestamps",
    "VK_EXT_color_write_enable",
    "VK_EXT_conditional_rendering",
    "VK_EXT_conservative_rasterization",
    "VK_EXT_custom_border_color",
    "VK_EXT_depth_bias_control",
    "VK_EXT_depth_clamp_control",
    "VK_EXT_depth_clamp_zero_one",
    "VK_EXT_depth_clip_control",
    "VK_EXT_depth_clip_enable",
    "VK_EXT_depth_range_unrestricted",
    "VK_EXT_descriptor_heap",
    "VK_EXT_dynamic_rendering_unused_attachments",
    "VK_EXT_extended_dynamic_state3",
    "VK_EXT_external_memory_acquire_unmodified",
    "VK_EXT_external_memory_dma_buf",
    "VK_EXT_filter_cubic",
    "VK_EXT_fragment_shader_interlock",
    "VK_EXT_global_priority",
    "VK_EXT_global_priority_query",
    "VK_EXT_graphics_pipeline_library",
    "VK_EXT_image_2d_view_of_3d",
    "VK_EXT_image_drm_format_modifier",
    "VK_EXT_image_sliced_view_of_3d",
    "VK_EXT_image_view_min_lod",
    "VK_EXT_index_type_uint8",
    "VK_EXT_legacy_dithering",
    "VK_EXT_legacy_vertex_attributes",
    "VK_EXT_line_rasterization",
    "VK_EXT_load_store_op_none",
    "VK_EXT_memory_budget",
    "VK_EXT_mesh_shader",
    "VK_EXT_multi_draw",
    "VK_EXT_multisampled_render_to_single_sampled",
    "VK_EXT_mutable_descriptor_type",
    "VK_EXT_nested_command_buffer",
    "VK_EXT_non_seamless_cube_map",
    "VK_EXT_pci_bus_info",
    "VK_EXT_pipeline_library_group_handles",
    "VK_EXT_post_depth_coverage",
    "VK_EXT_primitive_restart_index",
    "VK_EXT_primitive_topology_list_restart",
    "VK_EXT_primitives_generated_query",
    "VK_EXT_provoking_vertex",
    "VK_EXT_queue_family_foreign",
    "VK_EXT_rasterization_order_attachment_access",
    "VK_EXT_rgba10x6_formats",
    "VK_EXT_robustness2",
    "VK_EXT_sample_locations",
    "VK_EXT_shader_atomic_float",
    "VK_EXT_shader_atomic_float2",
    "VK_EXT_shader_float8",
    "VK_EXT_shader_image_atomic_int64",
    "VK_EXT_shader_replicated_composites",
    "VK_EXT_shader_split_barrier",
    "VK_EXT_shader_stencil_export",
    "VK_EXT_shader_subgroup_ballot",
    "VK_EXT_shader_subgroup_vote",
    "VK_EXT_shader_tile_image",
    "VK_EXT_shader_uniform_buffer_unsized_array",
    "VK_EXT_transform_feedback",
    "VK_EXT_vertex_attribute_divisor",
    "VK_EXT_vertex_attribute_robustness",
    "VK_EXT_vertex_input_dynamic_state",
    "VK_EXT_ycbcr_image_arrays",
    "VK_EXT_zero_initialize_device_memory",
    // vendor extensions
    "VK_ARM_rasterization_order_attachment_access",
    "VK_GOOGLE_decorate_string",
    "VK_GOOGLE_hlsl_functionality1",
    "VK_GOOGLE_user_type",
    "VK_IMG_filter_cubic",
    "VK_NV_compute_shader_derivatives",
    "VK_VALVE_mutable_descriptor_type",
];

/// `Gen.PRIMITIVE_TYPES`: the C scalar types that are serializable, with
/// their size in bytes.
pub const PRIMITIVE_TYPES: &[(&str, u32)] = &[
    ("float", 4),
    ("double", 8),
    ("uint8_t", 1),
    ("uint16_t", 2),
    ("uint32_t", 4),
    ("uint64_t", 8),
    ("int32_t", 4),
    ("int64_t", 8),
];

/// `Gen.UNION_DEFAULT_TAGS`: the member a union without selector information
/// is encoded as.
pub const UNION_DEFAULT_TAGS: &[(&str, u32)] = &[
    ("VkClearColorValue", 2),
    ("VkClearValue", 0),
    ("VkDeviceOrHostAddressKHR", 0),
    ("VkDeviceOrHostAddressConstKHR", 0),
    ("VkPipelineExecutableStatisticValueKHR", 2),
];

/// `Gen.COMMAND_BLOCK_LIST`.
pub const COMMAND_BLOCK_LIST: &[&str] = &[
    // Most VK_KHR_acceleration_structure host commands are blocked since
    // VkDeviceOrHostAddressKHR and VkDeviceOrHostAddressConstKHR have been
    // redirected to VkDeviceAddress. This avoids invalid helpers.
    "vkBuildAccelerationStructuresKHR",
    "vkCopyAccelerationStructureToMemoryKHR",
    "vkCopyMemoryToAccelerationStructureKHR",
    // vkWriteSamplerDescriptorMESA and vkWriteResourceDescriptorMESA are
    // used instead of adding complicated var_in_out support.
    "vkWriteSamplerDescriptorsEXT",
    "vkWriteResourceDescriptorsEXT",
];

/// `VkVariable` members that are encoded only when a condition holds, so the
/// renderer is not handed an array the API ignores.
#[derive(Clone, Copy, Debug)]
pub struct Ignorable {
    pub struct_name: &'static str,
    pub member: &'static str,
    pub condition: Condition,
}

/// The `condition` strings of `IGNORABLE_LIST`, typed. Both shapes read one
/// member of the struct being encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    /// `val->member == VALUE`
    Equals {
        member: &'static str,
        value: &'static str,
    },
    /// `!(val->member & BIT)`
    FlagClear {
        member: &'static str,
        bit: &'static str,
    },
}

impl Condition {
    /// The C text of the condition, as the Python literal spells it.
    pub fn c_text(&self) -> String {
        match self {
            Self::Equals { member, value } => format!("val->{member} == {value}"),
            Self::FlagClear { member, bit } => format!("!(val->{member} & {bit})"),
        }
    }
}

/// `Gen.IGNORABLE_LIST`.
pub const IGNORABLE_LIST: &[Ignorable] = &[
    Ignorable {
        struct_name: "VkImageCreateInfo",
        member: "pQueueFamilyIndices",
        condition: Condition::Equals {
            member: "sharingMode",
            value: "VK_SHARING_MODE_CONCURRENT",
        },
    },
    Ignorable {
        struct_name: "VkBufferCreateInfo",
        member: "pQueueFamilyIndices",
        condition: Condition::Equals {
            member: "sharingMode",
            value: "VK_SHARING_MODE_CONCURRENT",
        },
    },
    Ignorable {
        struct_name: "VkPhysicalDeviceImageDrmFormatModifierInfoEXT",
        member: "pQueueFamilyIndices",
        condition: Condition::Equals {
            member: "sharingMode",
            value: "VK_SHARING_MODE_CONCURRENT",
        },
    },
    Ignorable {
        struct_name: "VkFramebufferCreateInfo",
        member: "pAttachments",
        condition: Condition::FlagClear {
            member: "flags",
            bit: "VK_FRAMEBUFFER_CREATE_IMAGELESS_BIT",
        },
    },
];

/// `TYPE_BLOCK_LIST` of `vkxml.py`.
pub const TYPE_BLOCK_LIST: &[&str] = &[
    // Block layering structs from VK_KHR_maintenance7 as the driver will
    // fill those with core property structs.
    "VkPhysicalDeviceLayeredApiPropertiesListKHR",
    "VkPhysicalDeviceLayeredApiPropertiesKHR",
    "VkPhysicalDeviceLayeredApiKHR",
    "VkPhysicalDeviceLayeredApiVulkanPropertiesKHR",
];

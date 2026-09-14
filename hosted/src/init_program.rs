#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// The payload `kernel-prebuild` wrote for this host, named in the
    /// manifest the Justfile exports for the test.
    fn test_payload() -> helios_kernel::BootPayload {
        let manifest_path = PathBuf::from(
            std::env::var_os("HELIOS_KERNEL_PREBUILD_MANIFEST")
                .expect("HELIOS_KERNEL_PREBUILD_MANIFEST must name the kernel-prebuild manifest"),
        );
        let manifest = std::fs::read(&manifest_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest_path.display()));
        let manifest: serde_json::Value =
            serde_json::from_slice(&manifest).unwrap_or_else(|error| {
                panic!("failed to decode {}: {error}", manifest_path.display())
            });
        let bootfs = manifest["bootfs"]
            .as_str()
            .expect("kernel-prebuild manifest must carry a bootfs path");
        let bytes = std::fs::read(bootfs)
            .unwrap_or_else(|error| panic!("failed to read {bootfs}: {error}"));
        helios_kernel::BootPayload::parse(Box::leak(bytes.into_boxed_slice()))
            .unwrap_or_else(|error| panic!("{bootfs} is not a valid payload: {error}"))
    }

    #[test]
    fn embedded_debugger_is_provisioned() {
        let component = test_payload()
            .system_component()
            .expect("kernel prebuild bootfs must carry /bin/debugger");
        assert_eq!(component.name(), "bin/debugger");
        assert!(
            !component.bytes().is_empty(),
            "embedded debugger payload must not be empty"
        );
    }
}

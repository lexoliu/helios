#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// The payload `kernel-prebuild` wrote for this host, named in the
    /// `HELIOS_BOOTFS` path the Justfile exports for the test.
    fn test_payload() -> helios_kernel::BootPayload {
        let bootfs = PathBuf::from(
            std::env::var_os("HELIOS_BOOTFS")
                .expect("HELIOS_BOOTFS must name the kernel-prebuild payload"),
        );
        let bytes = std::fs::read(&bootfs)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", bootfs.display()));
        helios_kernel::BootPayload::parse(Box::leak(bytes.into_boxed_slice()))
            .unwrap_or_else(|error| panic!("{} is not a valid payload: {error}", bootfs.display()))
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

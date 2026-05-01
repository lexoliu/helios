#[cfg(test)]
mod tests {
    #[test]
    fn embedded_debugger_is_provisioned() {
        let component = helios_kernel::embedded_system_component()
            .expect("kernel prebuild manifest must embed /bin/debugger");
        assert_eq!(component.name(), "bin/debugger");
        assert!(
            !component.bytes().is_empty(),
            "embedded debugger payload must not be empty"
        );
    }
}

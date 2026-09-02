//! Typed construction of QEMU option lists.
//!
//! Every `-netdev`, `-device`, `-drive`, and `-object` argument QEMU
//! accepts is a bare type token followed by comma-separated `key=value`
//! pairs. Building those by hand with `format!` chains puts the layout of
//! a structured argument inside string literals scattered across call
//! sites, where a renamed key silently produces an option QEMU rejects at
//! runtime. [`QemuOptions`] keeps the shape in one place: callers name
//! keys and values, and the list renders itself.

use std::fmt;

/// A QEMU option list: a bare type token plus ordered `key=value` pairs.
///
/// Order is preserved because QEMU reports option errors positionally and
/// a stable rendering keeps the inspector's own unit tests readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QemuOptions {
    kind: Option<String>,
    entries: Vec<(String, String)>,
}

impl QemuOptions {
    /// Starts an option list for the given QEMU type token, for example
    /// `virtio-net-pci` or `tap`.
    pub(crate) fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: Some(kind.into()),
            entries: Vec::new(),
        }
    }

    /// Starts an option list with no leading type token, as `-drive`
    /// takes.
    pub(crate) fn keyed() -> Self {
        Self {
            kind: None,
            entries: Vec::new(),
        }
    }

    /// Appends `key=value`, replacing an earlier value for the same key.
    ///
    /// Replacement keeps the original position so a value derived from a
    /// later decision cannot reorder an option list that a test pins.
    pub(crate) fn set(&mut self, key: impl Into<String>, value: impl fmt::Display) -> &mut Self {
        let key = key.into();
        let value = value.to_string();
        match self.entries.iter_mut().find(|(name, _)| *name == key) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((key, value)),
        }
        self
    }

    /// Whether the list already carries a value for `key`.
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(name, _)| name == key)
    }
}

impl fmt::Display for QemuOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut separator = "";
        if let Some(kind) = &self.kind {
            formatter.write_str(kind)?;
            separator = ",";
        }
        for (key, value) in &self.entries {
            write!(formatter, "{separator}{key}={value}")?;
            separator = ",";
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::QemuOptions;

    #[test]
    fn renders_the_type_token_alone_without_entries() {
        assert_eq!(QemuOptions::new("user").to_string(), "user");
    }

    #[test]
    fn renders_entries_in_insertion_order() {
        let mut options = QemuOptions::new("tap");
        options.set("id", "net0").set("ifname", "helios0");
        options.set("queues", 4u16);
        assert_eq!(options.to_string(), "tap,id=net0,ifname=helios0,queues=4");
    }

    #[test]
    fn a_keyed_list_renders_without_a_leading_separator() {
        let mut options = QemuOptions::keyed();
        options.set("if", "none").set("format", "raw");
        assert_eq!(options.to_string(), "if=none,format=raw");
    }

    #[test]
    fn replacing_a_key_keeps_its_original_position() {
        let mut options = QemuOptions::new("virtio-net-pci");
        options
            .set("netdev", "net0")
            .set("mq", "on")
            .set("mq", "off");
        assert_eq!(options.to_string(), "virtio-net-pci,netdev=net0,mq=off");
        assert!(options.contains("mq"));
        assert!(!options.contains("vectors"));
    }
}

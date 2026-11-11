//! The environment a launched program observes.

use crate::InstanceId;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The variable that names a program's own instance. The kernel owns it.
const HELIOS_PROCESS_ID_ENV: &str = "HELIOS_PROCESS_ID";

/// A launch's environment before the kernel has named the instance.
///
/// `HELIOS_PROCESS_ID` is the kernel's to set, so a caller's entry for it
/// is not carried: [`Self::from_guest`] drops it and
/// [`Self::into_process_env`] appends the kernel's value, whichever launch
/// path built the environment. A launch therefore cannot reach the
/// launcher with the name already present, and the child observes exactly
/// one entry, the kernel's.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct LaunchEnvironment(Vec<(String, String)>);

impl LaunchEnvironment {
    /// The environment a guest supplied or inherited, with the
    /// kernel-owned name removed.
    pub(super) fn from_guest(mut environment: Vec<(String, String)>) -> Self {
        environment.retain(|(name, _)| name.as_str() != HELIOS_PROCESS_ID_ENV);
        Self(environment)
    }

    /// The environment of the instance `instance`, with the kernel's
    /// `HELIOS_PROCESS_ID` appended.
    pub(super) fn into_process_env(self, instance: InstanceId) -> Vec<(String, String)> {
        let mut environment = self.0;
        environment.push((HELIOS_PROCESS_ID_ENV.into(), instance.raw().to_string()));
        environment
    }
}

#[cfg(test)]
mod tests {
    use super::{HELIOS_PROCESS_ID_ENV, LaunchEnvironment};
    use crate::InstanceId;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;

    fn entry(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    fn process_ids(environment: &[(String, String)]) -> Vec<&str> {
        environment
            .iter()
            .filter(|(name, _)| name == HELIOS_PROCESS_ID_ENV)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// A guest-named `HELIOS_PROCESS_ID` is replaced by the kernel's value:
    /// the child sees one entry, and the other entries are untouched and in
    /// order. This is the rule behind `helios:system/programs` `spawn` and
    /// `exec`, the WASIX spawn and exec calls, and fork.
    #[test]
    fn guest_process_id_is_replaced_by_the_kernels() {
        let environment = LaunchEnvironment::from_guest(vec![
            entry("PATH", "/bin"),
            entry(HELIOS_PROCESS_ID_ENV, "forged"),
            entry("HOME", "/root"),
            entry(HELIOS_PROCESS_ID_ENV, "forged-again"),
        ])
        .into_process_env(InstanceId::from_raw(42));
        assert_eq!(process_ids(&environment), ["42"]);
        assert_eq!(
            environment,
            vec![
                entry("PATH", "/bin"),
                entry("HOME", "/root"),
                entry(HELIOS_PROCESS_ID_ENV, "42"),
            ]
        );
    }

    /// An environment that does not name the variable gains exactly the
    /// kernel's entry.
    #[test]
    fn kernel_process_id_is_added_once() {
        let environment = LaunchEnvironment::from_guest(vec![entry("PATH", "/bin")])
            .into_process_env(InstanceId::from_raw(7));
        assert_eq!(
            environment,
            vec![entry("PATH", "/bin"), entry(HELIOS_PROCESS_ID_ENV, "7")]
        );
    }
}

//! The argument vector a launched program observes.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A program's complete argument vector, `argv[0]` included.
///
/// POSIX makes `argv[0]` the caller's business: a shell that resolved
/// `PATH` itself execs the resolved path but passes the bare name it was
/// typed with, while a launch by path passes that path. The kernel
/// therefore never rewrites, strips, or re-prepends `argv[0]` on behalf of
/// a caller. The only argv the kernel assembles is the one for a program it
/// launches itself, where there is no caller to name it — see
/// [`ProgramArgv::launched`].
///
/// The type stores `argv[0]` apart from `argv[1..]` so "an argv always has a
/// program name" is a structural property rather than a runtime assertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProgramArgv {
    /// `argv[0]`: the name the program was invoked under.
    program_name: String,
    /// `argv[1..]`.
    arguments: Vec<String>,
}

impl ProgramArgv {
    /// The argument vector exactly as a caller supplied it. `argv[0]` is
    /// passed through untouched, so a program launched by bare name sees the
    /// bare name and one launched by path sees the path.
    ///
    /// `program_name` is the name the kernel resolved the executable under
    /// and is used only when the caller supplied no arguments at all: such a
    /// caller named nothing, so the resolved name stands in for `argv[0]`.
    pub(crate) fn from_caller(program_name: &str, argv: Vec<String>) -> Self {
        let mut argv = argv.into_iter();
        match argv.next() {
            Some(caller_name) => Self {
                program_name: caller_name,
                arguments: argv.collect(),
            },
            None => Self::launched(program_name, Vec::new()),
        }
    }

    /// The argument vector for a program the kernel launches itself, where
    /// no caller supplied an `argv[0]`: `program_name` becomes `argv[0]` and
    /// `arguments` becomes `argv[1..]`.
    pub(crate) fn launched(program_name: impl ToString, arguments: Vec<String>) -> Self {
        Self {
            program_name: program_name.to_string(),
            arguments,
        }
    }

    /// Recover an argv from its flattened form, for a child that inherits
    /// its parent's argument vector verbatim (`fork`). `None` when the
    /// flattened vector carries no `argv[0]`.
    pub(crate) fn inherited(argv: Vec<String>) -> Option<Self> {
        let mut argv = argv.into_iter();
        let program_name = argv.next()?;
        Some(Self {
            program_name,
            arguments: argv.collect(),
        })
    }

    /// `argv[0]`, which also names the instance in diagnostics.
    pub(crate) fn program_name(&self) -> &str {
        &self.program_name
    }

    /// The flattened argument vector handed to the guest.
    pub(crate) fn into_vec(self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.arguments.len() + 1);
        argv.push(self.program_name);
        argv.extend(self.arguments);
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::ProgramArgv;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    /// dash resolves `PATH` itself: it execs `/bin/curl` but names the child
    /// `curl`, exactly as the user typed it. The resolved path must not leak
    /// into the guest's argv.
    #[test]
    fn bare_name_launch_keeps_the_caller_argv0() {
        let argv = ProgramArgv::from_caller(
            "/bin/curl",
            vec![
                "curl".to_string(),
                "http://detectportal.firefox.com/success.txt".to_string(),
            ],
        );
        assert_eq!(argv.program_name(), "curl");
        assert_eq!(
            argv.into_vec(),
            vec![
                "curl".to_string(),
                "http://detectportal.firefox.com/success.txt".to_string()
            ]
        );
    }

    /// A launch by path names the child by that path, and the path must
    /// appear exactly once.
    #[test]
    fn path_launch_keeps_the_caller_argv0() {
        let argv = ProgramArgv::from_caller(
            "/bin/curl",
            vec![
                "/bin/curl".to_string(),
                "http://detectportal.firefox.com/success.txt".to_string(),
            ],
        );
        assert_eq!(argv.program_name(), "/bin/curl");
        assert_eq!(
            argv.into_vec(),
            vec![
                "/bin/curl".to_string(),
                "http://detectportal.firefox.com/success.txt".to_string()
            ]
        );
    }

    /// A caller that supplied no arguments named nothing, so the resolved
    /// program name stands in for `argv[0]`.
    #[test]
    fn caller_without_arguments_is_named_by_the_resolved_program() {
        let argv = ProgramArgv::from_caller("/bin/curl", Vec::new());
        assert_eq!(argv.program_name(), "/bin/curl");
        assert_eq!(argv.into_vec(), vec!["/bin/curl".to_string()]);
    }

    /// The kernel-launched shape is the one place a name is synthesised.
    #[test]
    fn kernel_launched_argv_prepends_the_program_name() {
        let argv = ProgramArgv::launched("/bin/dash", vec!["-c".to_string(), "true".to_string()]);
        assert_eq!(argv.program_name(), "/bin/dash");
        assert_eq!(
            argv.into_vec(),
            vec![
                "/bin/dash".to_string(),
                "-c".to_string(),
                "true".to_string()
            ]
        );
    }
}

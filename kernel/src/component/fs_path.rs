extern crate alloc;

use alloc::string::String;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ComponentFsPathError {
    #[error("base path must be absolute")]
    InvalidBasePath,
    #[error("path is not permitted")]
    NotPermitted,
}

// Guest filesystem calls resolve descriptor-relative paths repeatedly. Keep this
// one-pass: local divan measured this shape at 135.2 ns / 1 allocation versus
// the old segment-Vec rebuild path's 453.8 ns / 10 allocations.
pub fn resolve_child_path(base: &str, child: &str) -> Result<String, ComponentFsPathError> {
    if child.starts_with('/') {
        return Err(ComponentFsPathError::NotPermitted);
    }

    let mut output =
        String::with_capacity(base.len().saturating_add(child.len()).saturating_add(1));
    push_absolute_segments(base, &mut output)?;
    // `..` may climb back through what `child` added, across any mount it
    // entered, but never above `base`: a descriptor-relative path stays
    // inside the directory the descriptor names.
    let floor = output.len();
    push_relative_segments(child, Some(floor), &mut output)?;
    Ok(finalize_absolute_path(output))
}

pub fn resolve_guest_path(base: &str, path: &str) -> Result<String, ComponentFsPathError> {
    if path.starts_with('/') {
        return resolve_absolute_path(path);
    }
    resolve_child_path(base, path)
}

pub fn resolve_absolute_path(path: &str) -> Result<String, ComponentFsPathError> {
    let mut output = String::with_capacity(path.len());
    push_absolute_segments(path, &mut output)?;
    Ok(finalize_absolute_path(output))
}

pub fn directory_prefix(path: &str) -> String {
    if path == "/" {
        return String::from("/");
    }
    let mut prefix = String::with_capacity(path.len() + 1);
    prefix.push_str(path);
    prefix.push('/');
    prefix
}

// Directory membership checks run while scanning embedded FS snapshots. Avoid
// allocating `directory_prefix(path)` for every read-directory/remove/rename
// pass: local divan measured 2.93 ns / zero allocation versus 22.88 ns / 25 B.
pub fn path_is_within_directory(path: &str, directory: &str) -> bool {
    path != directory && strip_directory_prefix(path, directory).is_some()
}

pub fn strip_directory_prefix<'a>(path: &'a str, directory: &str) -> Option<&'a str> {
    if directory == "/" {
        return path
            .strip_prefix('/')
            .filter(|remainder| !remainder.is_empty());
    }
    path.strip_prefix(directory)
        .and_then(|remainder| remainder.strip_prefix('/'))
        .filter(|remainder| !remainder.is_empty())
}

pub fn parent_path(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((parent, _)) => parent,
    }
}

fn push_absolute_segments(path: &str, output: &mut String) -> Result<(), ComponentFsPathError> {
    if !path.starts_with('/') {
        return Err(ComponentFsPathError::InvalidBasePath);
    }
    // An absolute path names what it names: authority checks compare it
    // segment by segment, so it is never normalized through `..`.
    push_relative_segments(path, None, output)
}

/// Appends `path`'s segments to `output`, resolving `.` lexically. With a
/// `floor`, `..` removes the last segment but none of the first `floor`
/// bytes of `output`; without one, `..` is refused.
fn push_relative_segments(
    path: &str,
    floor: Option<usize>,
    output: &mut String,
) -> Result<(), ComponentFsPathError> {
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            if floor.is_none_or(|floor| output.len() <= floor) {
                return Err(ComponentFsPathError::NotPermitted);
            }
            let parent = output
                .rfind('/')
                .expect("every pushed segment starts with /");
            output.truncate(parent);
            continue;
        }
        output.push('/');
        output.push_str(segment);
    }
    Ok(())
}

fn finalize_absolute_path(path: String) -> String {
    if path.is_empty() {
        return String::from("/");
    }
    path
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{
        ComponentFsPathError, directory_prefix, path_is_within_directory, resolve_absolute_path,
        resolve_child_path, resolve_guest_path, strip_directory_prefix,
    };

    #[test]
    fn child_path_resolution_rejects_absolute_paths() {
        let error = resolve_child_path("/sandbox", "/etc/passwd")
            .expect_err("child path resolution must not accept rooted input");

        assert_eq!(error, ComponentFsPathError::NotPermitted);
    }

    #[test]
    fn child_path_resolution_rejects_parent_escape() {
        let error = resolve_child_path("/sandbox", "../escape")
            .expect_err("child path resolution must not accept parent traversal");

        assert_eq!(error, ComponentFsPathError::NotPermitted);
    }

    #[test]
    fn child_path_resolution_climbs_back_through_its_own_segments() {
        assert_eq!(
            resolve_child_path("/sandbox", "a/b/../../c/./d/.."),
            Ok(String::from("/sandbox/c"))
        );
        assert_eq!(resolve_child_path("/", "a/.."), Ok(String::from("/")));
        assert_eq!(
            resolve_child_path("/sandbox", "a/../.."),
            Err(ComponentFsPathError::NotPermitted)
        );
    }

    #[test]
    fn child_path_resolution_keeps_base_confinement() {
        let path = resolve_child_path("/sandbox", "bin/./tool")
            .expect("relative child path should resolve inside the base");

        assert_eq!(path, "/sandbox/bin/tool");
    }

    #[test]
    fn guest_path_resolution_allows_absolute_program_paths() {
        let path = resolve_guest_path("/cwd", "/bin/tool")
            .expect("program artifact paths may be absolute before authority checks");

        assert_eq!(path, "/bin/tool");
    }

    #[test]
    fn absolute_path_resolution_rejects_parent_segments() {
        let error = resolve_absolute_path("/bin/../secret")
            .expect_err("absolute authority paths must not normalize through parent traversal");

        assert_eq!(error, ComponentFsPathError::NotPermitted);
    }

    #[test]
    fn directory_prefix_preserves_path_boundaries() {
        assert_eq!(directory_prefix("/"), "/");
        assert_eq!(directory_prefix("/bin"), "/bin/");
    }

    #[test]
    fn strip_directory_prefix_preserves_path_boundaries_without_allocating() {
        assert_eq!(strip_directory_prefix("/bin/tool", "/bin"), Some("tool"));
        assert_eq!(strip_directory_prefix("/bin", "/bin"), None);
        assert_eq!(strip_directory_prefix("/binary/tool", "/bin"), None);
        assert_eq!(strip_directory_prefix("/bin/tool", "/"), Some("bin/tool"));
        assert!(path_is_within_directory("/bin/tool", "/bin"));
        assert!(!path_is_within_directory("/binary/tool", "/bin"));
    }
}

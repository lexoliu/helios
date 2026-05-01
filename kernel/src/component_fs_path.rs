extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ComponentFsPathError {
    #[error("base path must be absolute")]
    InvalidBasePath,
    #[error("path is not permitted")]
    NotPermitted,
}

pub fn resolve_child_path(base: &str, child: &str) -> Result<String, ComponentFsPathError> {
    if child.starts_with('/') {
        return Err(ComponentFsPathError::NotPermitted);
    }

    let mut segments = split_absolute_path(base)?;
    for segment in child.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(ComponentFsPathError::NotPermitted);
        }
        segments.push(segment.to_owned());
    }
    Ok(build_absolute_path(&segments))
}

pub fn resolve_guest_path(base: &str, path: &str) -> Result<String, ComponentFsPathError> {
    if path.starts_with('/') {
        return resolve_absolute_path(path);
    }
    resolve_child_path(base, path)
}

pub fn resolve_absolute_path(path: &str) -> Result<String, ComponentFsPathError> {
    let segments = split_absolute_path(path)?;
    Ok(build_absolute_path(&segments))
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

pub fn parent_path(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((parent, _)) => parent,
    }
}

fn split_absolute_path(path: &str) -> Result<Vec<String>, ComponentFsPathError> {
    if !path.starts_with('/') {
        return Err(ComponentFsPathError::InvalidBasePath);
    }

    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(ComponentFsPathError::NotPermitted);
        }
        segments.push(segment.to_owned());
    }
    Ok(segments)
}

fn build_absolute_path(segments: &[String]) -> String {
    if segments.is_empty() {
        return String::from("/");
    }

    let mut path = String::new();
    for segment in segments {
        path.push('/');
        path.push_str(segment);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::{
        ComponentFsPathError, directory_prefix, resolve_absolute_path, resolve_child_path,
        resolve_guest_path,
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
}

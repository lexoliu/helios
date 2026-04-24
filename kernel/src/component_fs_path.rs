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
    alloc::format!("{path}/")
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

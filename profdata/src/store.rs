//! Where the profile a release published lives once it is fetched.
//!
//! One profile is in force at a time — the one the latest release
//! carries — but a profile is keyed by the release it came from, because
//! a kernel built against a profile is only as good as the profile
//! behind it and "which release was that" is part of the answer. The
//! store therefore keeps every fetched profile under its own tag and one
//! record naming the tag in force:
//!
//! ```text
//! target/profiles/fetched.json          the release in force
//! target/profiles/<tag>/helios-kernel.profdata
//! ```
//!
//! It lives under `target/` because it is a build input a checkout can
//! reproduce by fetching again, never a source file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{FETCH_COMMAND, KERNEL_PROFILE_ASSET, ProfileUseError, validate};

/// The store's directory under `target/`.
const STORE_DIRECTORY: &str = "profiles";

/// The record naming the release whose profile is in force.
const RECORD_FILE: &str = "fetched.json";

/// The release a stored profile came from.
///
/// Written by `helios-cli profile-fetch` and read by every build that
/// spends the profile, so a kernel image can be traced back to the
/// release whose counts shaped it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedProfile {
    /// `owner/name` of the repository the release was published in.
    pub repository: String,
    /// The release's tag, which is also the directory the profile is in.
    pub tag: String,
}

/// Why the fetched kernel profile could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum KernelProfileStoreError {
    /// Nothing has been fetched into this checkout's store yet.
    #[error(
        "no kernel profile has been fetched into {record}; `{FETCH_COMMAND}` downloads the \
         {KERNEL_PROFILE_ASSET} asset that release.yml's kernel-profile job attaches to \
         every release (docs/pgo.md)"
    )]
    NotFetched {
        /// The record that is not there.
        record: String,
    },
    /// The record exists but could not be read or written.
    #[error("failed to {action} {path}: {source}")]
    Record {
        /// What was being done to the record.
        action: &'static str,
        /// The record's path.
        path: String,
        /// The underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// The record is not the JSON this store writes.
    #[error("{path} is not a kernel-profile record: {source}; re-run `{FETCH_COMMAND}`")]
    ParseRecord {
        /// The record that could not be parsed.
        path: String,
        /// What serde made of it.
        #[source]
        source: serde_json::Error,
    },
    /// The record names a release whose profile is not in the store.
    #[error(
        "the store records the profile of release {tag} and {path} is not there; \
         re-run `{FETCH_COMMAND}`"
    )]
    ProfileMissing {
        /// The release the record names.
        tag: String,
        /// Where its profile should have been.
        path: String,
    },
    /// A tag that is not a single path segment cannot key a directory.
    #[error("the release tag {tag} is not a single path segment and cannot key the profile store")]
    TagNotASegment {
        /// The tag that was refused.
        tag: String,
    },
    /// The stored profile is not one this toolchain can build against.
    #[error("{0}")]
    Profile(#[from] ProfileUseError),
}

/// The fetched kernel profiles of one checkout.
#[derive(Debug, Clone)]
pub struct KernelProfileStore {
    directory: PathBuf,
}

impl KernelProfileStore {
    /// The store of the checkout rooted at `repo_root`.
    pub fn new(repo_root: &Path) -> Self {
        Self {
            directory: repo_root.join("target").join(STORE_DIRECTORY),
        }
    }

    /// The directory the store keeps everything in.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Where the profile of the release tagged `tag` is kept.
    pub fn profile_path(&self, tag: &str) -> Result<PathBuf, KernelProfileStoreError> {
        if tag.is_empty() || tag.contains(['/', '\\']) || tag.starts_with('.') {
            return Err(KernelProfileStoreError::TagNotASegment {
                tag: tag.to_owned(),
            });
        }
        Ok(self.directory.join(tag).join(KERNEL_PROFILE_ASSET))
    }

    /// The record naming the release whose profile is in force.
    pub fn record_path(&self) -> PathBuf {
        self.directory.join(RECORD_FILE)
    }

    /// Records `profile` as the release in force.
    ///
    /// Called once its profile is in the store and has passed the header
    /// check, so a record never names a profile a build cannot read.
    pub fn publish(&self, profile: &FetchedProfile) -> Result<(), KernelProfileStoreError> {
        let path = self.record_path();
        let record = |action: &'static str| {
            let path = path.display().to_string();
            move |source| KernelProfileStoreError::Record {
                action,
                path,
                source,
            }
        };
        fs::create_dir_all(&self.directory).map_err(record("create"))?;
        let document = serde_json::to_string_pretty(profile)
            .expect("a repository and a tag serialise as JSON")
            + "\n";
        fs::write(&path, document).map_err(record("write"))
    }

    /// The release in force and the profile it published, header checked.
    ///
    /// Every failure names `helios-cli profile-fetch`: an empty store is
    /// the state a fresh checkout is in, and the answer to all of them is
    /// to fetch.
    pub fn fetched(&self) -> Result<(FetchedProfile, PathBuf), KernelProfileStoreError> {
        let record_path = self.record_path();
        let document = match fs::read_to_string(&record_path) {
            Ok(document) => document,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(KernelProfileStoreError::NotFetched {
                    record: record_path.display().to_string(),
                });
            }
            Err(source) => {
                return Err(KernelProfileStoreError::Record {
                    action: "read",
                    path: record_path.display().to_string(),
                    source,
                });
            }
        };
        let profile: FetchedProfile = serde_json::from_str(&document).map_err(|source| {
            KernelProfileStoreError::ParseRecord {
                path: record_path.display().to_string(),
                source,
            }
        })?;
        let path = self.profile_path(&profile.tag)?;
        if !path.is_file() {
            return Err(KernelProfileStoreError::ProfileMissing {
                tag: profile.tag,
                path: path.display().to_string(),
            });
        }
        validate(&path)?;
        Ok((profile, path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::pinned_header;

    fn store_with_profile(root: &Path, tag: &str) -> KernelProfileStore {
        let store = KernelProfileStore::new(root);
        let path = store
            .profile_path(tag)
            .expect("a tag that keys a directory");
        fs::create_dir_all(path.parent().expect("the tag's directory"))
            .expect("creating the tag's directory");
        fs::write(&path, pinned_header()).expect("writing a profile header");
        store
    }

    #[test]
    fn a_published_record_reads_back_with_its_profile() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = store_with_profile(directory.path(), "helios-v0.1.0");
        let profile = FetchedProfile {
            repository: "lexoliu/helios".to_owned(),
            tag: "helios-v0.1.0".to_owned(),
        };
        store.publish(&profile).expect("publishing the record");
        let (read, path) = store
            .fetched()
            .expect("the record names a readable profile");
        assert_eq!(read, profile);
        assert_eq!(path, store.profile_path("helios-v0.1.0").expect("the path"));
    }

    #[test]
    fn an_empty_store_names_the_fetch_command() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        let error = store
            .fetched()
            .expect_err("a checkout that has fetched nothing has no profile");
        assert!(
            matches!(error, KernelProfileStoreError::NotFetched { .. }),
            "{error}"
        );
        assert!(error.to_string().contains(FETCH_COMMAND));
    }

    #[test]
    fn a_record_without_its_profile_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        store
            .publish(&FetchedProfile {
                repository: "lexoliu/helios".to_owned(),
                tag: "helios-v0.1.0".to_owned(),
            })
            .expect("publishing the record");
        let error = store
            .fetched()
            .expect_err("a record whose profile was deleted names no readable profile");
        assert!(
            matches!(error, KernelProfileStoreError::ProfileMissing { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_tag_that_is_not_a_path_segment_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        let error = store
            .profile_path("../elsewhere")
            .expect_err("a tag keys one directory in the store and nothing above it");
        assert!(
            matches!(error, KernelProfileStoreError::TagNotASegment { .. }),
            "{error}"
        );
    }
}

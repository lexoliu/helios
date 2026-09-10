//! Where a fetched kernel profile lives.
//!
//! One profile is in force at a time, but a profile is keyed by where it
//! came from, because a kernel built against a profile is only as good as
//! the profile behind it and "which collection was that" is part of the
//! answer. The store therefore keeps every fetched profile under a key of
//! its own and one record naming the one in force:
//!
//! ```text
//! target/profiles/fetched.json          the profile in force
//! target/profiles/<key>/helios-kernel.profdata
//! ```
//!
//! It lives under `target/` because it is a build input a checkout can
//! reproduce by fetching again, never a source file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    FETCH_COMMAND, KERNEL_PROFILE_ARTIFACT, KERNEL_PROFILE_ASSET, KERNEL_PROFILE_WORKFLOW,
    ProfileUseError, validate,
};

/// The store's directory under `target/`.
const STORE_DIRECTORY: &str = "profiles";

/// The record naming the profile in force.
const RECORD_FILE: &str = "fetched.json";

/// Digits of a commit hash a label carries: enough to name it, short
/// enough for a table cell.
const SHORT_SHA_LEN: usize = 7;

/// Where a stored profile came from.
///
/// Written by `helios-cli profile-fetch` and read by every build that
/// spends the profile, so a kernel image can be traced back to the
/// collection whose counts shaped it. The two sources are the two
/// producers of `docs/pgo.md`: a `kernel-profile.yml` run uploads the
/// profile as a workflow artifact, and `release.yml` attaches the one a
/// released kernel was built with to its release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum FetchedProfile {
    /// The `helios-kernel-profdata` artifact a `kernel-profile.yml` run
    /// uploaded (#313).
    Collection {
        /// `owner/name` of the repository the run was in.
        repository: String,
        /// The workflow run that uploaded the artifact.
        run_id: u64,
        /// The branch the run was on.
        head_branch: String,
        /// The commit the run collected on.
        head_sha: String,
    },
    /// The `helios-kernel.profdata` asset `release.yml` attached to a
    /// release (#226).
    Release {
        /// `owner/name` of the repository the release was published in.
        repository: String,
        /// The release's tag.
        tag: String,
    },
}

impl FetchedProfile {
    /// `owner/name` of the repository the profile came from.
    pub fn repository(&self) -> &str {
        match self {
            Self::Collection { repository, .. } | Self::Release { repository, .. } => repository,
        }
    }

    /// The directory the profile is kept under in the store: the release
    /// tag, or the run that uploaded the artifact.
    pub fn key(&self) -> String {
        match self {
            Self::Collection { run_id, .. } => format!("run-{run_id}"),
            Self::Release { tag, .. } => tag.clone(),
        }
    }

    /// The profile named short enough for a table cell and precise
    /// enough to find again: `release <tag>`, or the branch, the commit
    /// and the run of a collection.
    pub fn label(&self) -> String {
        match self {
            Self::Collection {
                run_id,
                head_branch,
                head_sha,
                ..
            } => {
                let short = head_sha.get(..SHORT_SHA_LEN).unwrap_or(head_sha);
                format!("{head_branch}@{short} run {run_id}")
            }
            Self::Release { tag, .. } => format!("release {tag}"),
        }
    }
}

/// Why the fetched kernel profile could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum KernelProfileStoreError {
    /// Nothing has been fetched into this checkout's store yet.
    #[error(
        "no kernel profile has been fetched into {record}; `{FETCH_COMMAND}` downloads the \
         {KERNEL_PROFILE_ARTIFACT} artifact the newest {KERNEL_PROFILE_WORKFLOW} run on the \
         default branch uploaded, or with --tag the {KERNEL_PROFILE_ASSET} asset of a release \
         (docs/pgo.md)"
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
    /// The record names a profile that is not in the store.
    #[error(
        "the store records the profile of {label} and {path} is not there; \
         re-run `{FETCH_COMMAND}`"
    )]
    ProfileMissing {
        /// The profile the record names.
        label: String,
        /// Where it should have been.
        path: String,
    },
    /// A key that is not a single path segment cannot name a directory.
    #[error("the profile key {key} is not a single path segment and cannot key the profile store")]
    KeyNotASegment {
        /// The key that was refused.
        key: String,
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

    /// Where the profile keyed `key` is kept.
    pub fn profile_path(&self, key: &str) -> Result<PathBuf, KernelProfileStoreError> {
        if key.is_empty() || key.contains(['/', '\\']) || key.starts_with('.') {
            return Err(KernelProfileStoreError::KeyNotASegment {
                key: key.to_owned(),
            });
        }
        Ok(self.directory.join(key).join(KERNEL_PROFILE_ASSET))
    }

    /// The record naming the profile in force.
    pub fn record_path(&self) -> PathBuf {
        self.directory.join(RECORD_FILE)
    }

    /// Records `profile` as the one in force.
    ///
    /// Called once its file is in the store and has passed the header
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
            .expect("a fetched profile serialises as JSON")
            + "\n";
        fs::write(&path, document).map_err(record("write"))
    }

    /// The profile in force and its file, header checked.
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
        let path = self.profile_path(&profile.key())?;
        if !path.is_file() {
            return Err(KernelProfileStoreError::ProfileMissing {
                label: profile.label(),
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

    fn release() -> FetchedProfile {
        FetchedProfile::Release {
            repository: "lexoliu/helios".to_owned(),
            tag: "helios-v0.1.0".to_owned(),
        }
    }

    fn collection() -> FetchedProfile {
        FetchedProfile::Collection {
            repository: "lexoliu/helios".to_owned(),
            run_id: 34_424_416_974,
            head_branch: "dev".to_owned(),
            head_sha: "85d20bcd0a1b2c3d4e5f60718293a4b5c6d7e8f9".to_owned(),
        }
    }

    fn store_with_profile(root: &Path, profile: &FetchedProfile) -> KernelProfileStore {
        let store = KernelProfileStore::new(root);
        let path = store
            .profile_path(&profile.key())
            .expect("a key that names a directory");
        fs::create_dir_all(path.parent().expect("the key's directory"))
            .expect("creating the key's directory");
        fs::write(&path, pinned_header()).expect("writing a profile header");
        store
    }

    #[test]
    fn a_published_record_reads_back_with_its_profile() {
        for profile in [release(), collection()] {
            let directory = tempfile::tempdir().expect("a temporary checkout");
            let store = store_with_profile(directory.path(), &profile);
            store.publish(&profile).expect("publishing the record");
            let (read, path) = store
                .fetched()
                .expect("the record names a readable profile");
            assert_eq!(read, profile);
            assert_eq!(path, store.profile_path(&profile.key()).expect("the path"));
        }
    }

    #[test]
    fn a_collection_is_keyed_by_its_run_and_labelled_by_where_it_ran() {
        assert_eq!(collection().key(), "run-34424416974");
        assert_eq!(collection().label(), "dev@85d20bc run 34424416974");
        assert_eq!(release().key(), "helios-v0.1.0");
        assert_eq!(release().label(), "release helios-v0.1.0");
    }

    #[test]
    fn the_record_says_which_source_it_names() {
        let document = serde_json::to_string(&collection()).expect("a record serialises");
        assert!(document.contains("\"source\":\"collection\""), "{document}");
        let document = serde_json::to_string(&release()).expect("a record serialises");
        assert!(document.contains("\"source\":\"release\""), "{document}");
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
        assert!(error.to_string().contains(KERNEL_PROFILE_WORKFLOW));
    }

    #[test]
    fn a_record_without_its_profile_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        store.publish(&release()).expect("publishing the record");
        let error = store
            .fetched()
            .expect_err("a record whose profile was deleted names no readable profile");
        assert!(
            matches!(error, KernelProfileStoreError::ProfileMissing { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("release helios-v0.1.0"));
    }

    #[test]
    fn a_key_that_is_not_a_path_segment_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        let error = store
            .profile_path("../elsewhere")
            .expect_err("a key names one directory in the store and nothing above it");
        assert!(
            matches!(error, KernelProfileStoreError::KeyNotASegment { .. }),
            "{error}"
        );
    }
}

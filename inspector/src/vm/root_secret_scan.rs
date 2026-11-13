//! The kernel image's root-secret scan.
//!
//! A kernel image carries the build root as a public key only: the secret
//! signs the bootfs on the host and is never linked into a guest image. The
//! scan reads the linked image once and fails the build when any form of the
//! secret appears in it, so a regression that leaks the key into the image
//! stops the build instead of shipping.

use std::fmt;
use std::path::Path;

use ed25519_dalek::SigningKey;
use ed25519_dalek::hazmat::ExpandedSecretKey;
use helios_workspace_root::root_key::{RootSecretReadError, read_root_secret_seed};
use memchr::memmem;

use super::kernel_image::{self, KernelSymbolsError};

/// A byte string the secret can be embedded as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootSecretForm {
    /// The 32-byte Ed25519 seed, the key file's content.
    Seed,
    /// The first half of the seed's SHA-512 expansion, before clamping.
    ExpansionScalarHalf,
    /// That half with the Ed25519 clamping bits applied.
    ClampedScalar,
    /// The clamped scalar reduced modulo the group order, as
    /// `ed25519-dalek` holds it for signing.
    ReducedScalar,
    /// The second half of the seed's SHA-512 expansion, the nonce prefix.
    ExpansionHashPrefix,
}

impl fmt::Display for RootSecretForm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Seed => "the 32-byte Ed25519 seed",
            Self::ExpansionScalarHalf => "the first half of the SHA-512 expansion",
            Self::ClampedScalar => "the clamped scalar",
            Self::ReducedScalar => "the clamped scalar reduced modulo the group order",
            Self::ExpansionHashPrefix => "the second half of the SHA-512 expansion",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RootSecretScanError {
    #[error("{0}")]
    Key(#[from] RootSecretReadError),
    #[error("{0}")]
    Image(#[from] KernelSymbolsError),
    #[error(
        "kernel image {image} contains the root secret key: {form} is at byte offset {offset}; \
         the image was searched for the Ed25519 seed and for the clamped scalar, the reduced \
         scalar and both halves of the SHA-512 expansion, and a guest image carries the public \
         key only"
    )]
    ContainsRootSecret {
        image: String,
        form: RootSecretForm,
        offset: usize,
    },
}

/// Every byte string the secret `seed` can appear as in an image.
fn forms(seed: [u8; 32]) -> [(RootSecretForm, [u8; 32]); 5] {
    let signing_key = SigningKey::from_bytes(&seed);
    let expansion_scalar_half = signing_key.to_scalar_bytes();
    let mut clamped = expansion_scalar_half;
    // RFC 8032 section 5.1.5: clear the low three bits, clear the top bit,
    // set the second-highest.
    clamped[0] &= 0b1111_1000;
    clamped[31] &= 0b0111_1111;
    clamped[31] |= 0b0100_0000;
    let expanded = ExpandedSecretKey::from(&seed);
    [
        (RootSecretForm::Seed, seed),
        (RootSecretForm::ExpansionScalarHalf, expansion_scalar_half),
        (RootSecretForm::ClampedScalar, clamped),
        (RootSecretForm::ReducedScalar, expanded.scalar.to_bytes()),
        (RootSecretForm::ExpansionHashPrefix, expanded.hash_prefix),
    ]
}

/// The first place a form of the secret in `secret_key` appears in the
/// image at `image`, as an error; `Ok` when the image holds none.
pub(crate) fn scan_image(image: &Path, secret_key: &Path) -> Result<(), RootSecretScanError> {
    let seed = read_root_secret_seed(secret_key)?;
    let bytes = kernel_image::read_image(image)?;
    scan_bytes(image, &bytes, seed)
}

fn scan_bytes(image: &Path, bytes: &[u8], seed: [u8; 32]) -> Result<(), RootSecretScanError> {
    let hit = forms(seed)
        .into_iter()
        .filter_map(|(form, needle)| memmem::find(bytes, &needle).map(|offset| (offset, form)))
        .min_by_key(|(offset, _)| *offset);
    match hit {
        Some((offset, form)) => Err(RootSecretScanError::ContainsRootSecret {
            image: image.display().to_string(),
            form,
            offset,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use helios_workspace_root::root_key::SECRET_KEY_MODE;

    use super::*;

    const SEED: [u8; 32] = [0x5a; 32];

    fn image_with(form: RootSecretForm, at: usize) -> Vec<u8> {
        // Filler that holds no form of the secret.
        let mut image = vec![0xc3_u8; 4096];
        let (_, needle) = forms(SEED)
            .into_iter()
            .find(|(candidate, _)| *candidate == form)
            .expect("every form is listed");
        image[at..at + 32].copy_from_slice(&needle);
        image
    }

    fn refusal(image: &[u8]) -> (RootSecretForm, usize) {
        match scan_bytes(Path::new("kernel.elf"), image, SEED) {
            Err(RootSecretScanError::ContainsRootSecret { form, offset, .. }) => (form, offset),
            other => panic!("expected the image to be refused, got {other:?}"),
        }
    }

    #[test]
    fn an_image_holding_the_seed_is_refused_with_its_offset() {
        assert_eq!(
            refusal(&image_with(RootSecretForm::Seed, 1234)),
            (RootSecretForm::Seed, 1234)
        );
    }

    #[test]
    fn an_image_holding_an_expanded_form_is_refused_with_its_offset() {
        for form in [
            RootSecretForm::ExpansionScalarHalf,
            RootSecretForm::ClampedScalar,
            RootSecretForm::ReducedScalar,
            RootSecretForm::ExpansionHashPrefix,
        ] {
            assert_eq!(refusal(&image_with(form, 77)), (form, 77));
        }
    }

    #[test]
    fn the_forms_of_a_secret_are_distinct() {
        let all = forms(SEED);
        for (index, (_, form)) in all.iter().enumerate() {
            for (_, other) in &all[index + 1..] {
                assert_ne!(form, other);
            }
        }
    }

    #[test]
    fn a_clean_image_passes() {
        let image = vec![0xc3_u8; 4096];
        scan_bytes(Path::new("kernel.elf"), &image, SEED).expect("no form of the secret");
    }

    #[test]
    fn another_keys_secret_in_the_image_is_not_this_keys() {
        let image = image_with(RootSecretForm::Seed, 0);
        scan_bytes(Path::new("kernel.elf"), &image, [0x33; 32])
            .expect("only the build's own secret is searched for");
    }

    #[test]
    fn the_scan_reads_the_key_file_and_the_image_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let key = dir.path().join("root.key");
        fs::write(&key, SEED).expect("a key file");
        fs::set_permissions(&key, fs::Permissions::from_mode(SECRET_KEY_MODE))
            .expect("an owner-only mode");
        let clean = dir.path().join("clean.elf");
        fs::write(&clean, vec![0xc3_u8; 4096]).expect("an image");
        scan_image(&clean, &key).expect("a clean image passes");
        let leaked = dir.path().join("leaked.elf");
        fs::write(&leaked, image_with(RootSecretForm::ClampedScalar, 9)).expect("an image");
        let error = scan_image(&leaked, &key).expect_err("the leaked image is refused");
        assert!(
            matches!(
                error,
                RootSecretScanError::ContainsRootSecret {
                    form: RootSecretForm::ClampedScalar,
                    offset: 9,
                    ..
                }
            ),
            "{error}"
        );
    }
}

use std::env;

/// The rustflag `.cargo/config.toml` sets for this target, as one argument.
const BRANCH_PROTECTION_FLAG: &str = "-Zbranch-protection=pac-ret";

/// Cargo separates the arguments of `CARGO_ENCODED_RUSTFLAGS` with this byte
/// (the Cargo book, "Environment variables Cargo sets for build scripts").
const ENCODED_RUSTFLAGS_SEPARATOR: char = '\x1f';

/// The backend enables the instruction key that every signed return address
/// in the image authenticates against. An image compiled without the flag
/// would carry no signed return address and the enable would protect nothing,
/// and the language offers no `cfg` that reports the flag, so the build
/// refuses it here.
fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    let flags = env::var("CARGO_ENCODED_RUSTFLAGS")
        .unwrap_or_else(|error| panic!("CARGO_ENCODED_RUSTFLAGS is unreadable: {error}"));
    assert!(
        flags
            .split(ENCODED_RUSTFLAGS_SEPARATOR)
            .any(|flag| flag == BRANCH_PROTECTION_FLAG),
        "helios-aarch64 signs return addresses and needs {BRANCH_PROTECTION_FLAG} in this \
         target's rustflags (.cargo/config.toml); got {flags:?}"
    );
}

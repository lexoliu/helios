# Boot protocol: the kernel image and the user payload

The kernel image and the user payload are separate artifacts. The kernel
binary carries the trusted root keys and nothing of the selected boot
programs; the payload is one file, `helios-bootfs`, produced by
`helios-cli kernel-prebuild` beside `kernel-prebuild.json`. Changing the
boot-program selection rewrites the payload and never recompiles the
kernel.

## The payload file

`helios-bootfs` is the format `artifact/src/bootfs.rs` defines: a fixed
header (magic, version, entry count, string-table and data offsets), an
entry table (kind, path offset/length, data offset/length,
`modified_nanos`), a string table, then the file bytes, every offset
16-byte aligned. The entry table holds every bootfs file and empty
directory plus two distinguished entries: the init component and its
`argv0`. The data section ends exactly where that body ends; a version-2
image is the body followed by a fixed 128-byte signature footer.

The footer holds the magic `HLBFSSIG`, a footer version, the algorithm
(Ed25519), the signer's public key, a 64-byte signature and reserved
zeros. The signature covers `b"helios-bootfs-signature-v1"` followed by
the SHA-256 of the body, so the header is covered too.
`helios-cli kernel-prebuild` signs the finished image once with the
prebuild root key. The init component, the compiler plugin and every
boot program inside it are bare Wasmtime artifacts with no per-artifact
signature trailer: the footer vouches for them as a whole.

The root key never lives in the repository or in an image, and the
inspector's image build enforces the second half: it searches the linked
kernel image for the key's seed and its expanded forms and fails the
build if any is there. A development
build generates it in its own prebuild directory and embeds its public
half in the kernel. A release names its key with `--root-secret-key
<file>` (on `helios-cli kernel-prebuild` and on the inspector's `vm`,
which forwards it): the file is read in place, and the release workflow
writes it from the repository secret `HELIOS_BOOTFS_ROOT_KEY`, the base64
of the 32 raw key bytes, to a file in the runner's temporary directory
and removes it after the upload. The signed payload ships beside the
kernel as the release asset `helios-bootfs-x86-64`, located with
`vm … bootfs-path`.

The kernel reads it with `BootPayload::parse`, the single entry on every
backend. It hashes the body once, verifies the footer against the
trusted root public keys embedded in the kernel image, and only then
parses the shape with `bootfs::Image::parse_signed`, a zero-copy reader
that validates every offset against the slice. The digest stays on the
`BootPayload` for a measurement register to extend. A refusal is a typed
`BootfsError` naming what was wrong: `MissingSignature`,
`UnsupportedSignature`, `UntrustedSigner` (with the signer's and the
trusted keys' fingerprints), `SignatureMismatch`, `TrailingBytes` or a
shape error, and each backend's panic message carries it. A bootfs file
is classified as a precompiled artifact by Wasmtime's own header check,
not by a trailer. A cwasm outside the payload (a file a guest wrote, one
on the host share) still carries the signature trailer and is verified
when it loads against the kernel's compiler-signing key, never against
the trusted roots (`kernel/src/signing_key.rs`).

## How each backend hands it over

- **x86-64 and aarch64** boot the Limine UEFI disk image
  `helios-cli limine-uefi-image` writes. The image places the payload at
  `/boot/helios-bootfs` and the entry's `module_path` names it, so it
  arrives as a Limine module. The backend takes the module whose path
  ends in `helios-bootfs` and panics naming every module present when
  none matches — a kernel without a payload has no init to run.
- **riscv64** boots `-kernel` directly; there is no Limine handoff. QEMU
  delivers the payload as the initrd (`-initrd`), publishes its physical
  range in the device tree's `/chosen` `linux,initrd-start` and
  `linux,initrd-end`, and the backend carves that range out of the
  memory the allocator is primed with before parsing it through the
  identity map.
- **hosted** takes the path on its command line: `helios --bootfs
  <path>`. The file is read once at start-up; absent or malformed is a
  hard error, the same contract the bare-metal paths enforce.

The parsed result is a `BootPayload`, which the backend hands
`RuntimeState::new`. `EmbeddedBootFs`, `EmbeddedBootFile`,
`EmbeddedBootDirectory`, `EmbeddedComponent` and `EmbeddedInit` stay the
kernel's view types — they are views over the payload image now instead
of generated statics.

## Booting a CI-built image without a rebuild

Each smoke lane uploads the release kernel and the full `helios-bootfs`
it built as `helios-guest-<arch>` (`aarch64`, `riscv64`, `x86-64`), kept
90 days. The archive mirrors the paths under `target/` that
`vm --no-build` reads, so on a host with the same commit checked out and
`helios-inspector` and `helios-cli` built, unpacking it into `target/` is
the whole installation:

```bash
gh run download <run> -n helios-guest-aarch64 -D target
./target/release/helios-inspector vm --arch aarch64 --release --no-build shell -c 'echo ok'
HELIOS_WORKLOAD_BENCH_NO_BUILD=1 tools/wasi-apps/workload-bench.sh
```

The archive holds `<triple>/kernel-release/helios` and
`kernel-prebuild/<triple>/kernel-release/helios-bootfs`; the x86-64 one
uses `profile-use` for the directory, because its release kernel is
built against the fetched profile, so that host runs
`helios-cli profile-fetch` first for the inspector to resolve the same
directory. The disk image Limine boots (x86-64, aarch64) is written at
boot from these two files, so the host needs `HELIOS_LIMINE_SHARE` and
`HELIOS_LIMINE_BIN` as for any boot. The prebuild manifest and the root
key files are not shipped: they are read by the build only, and the
manifest records absolute paths of the machine that wrote it.

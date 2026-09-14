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
`argv0`. The kernel parses it with `bootfs::Image::parse`, a zero-copy
reader that validates every offset against the slice and fails with a
typed error naming the field that was wrong.

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

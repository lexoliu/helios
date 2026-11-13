#!/usr/bin/env bash
# Reads what a measured-boot smoke boot left in its raw debug serial
# capture (`debug-serial.log`, which carries the console's colour
# escapes, so every search reads it as text).
#
#   measured-boot-check.sh settled <serial-log> <seed> <cause>
#       Asserts the boot logged exactly one "compiler-signing key
#       settled" line (kernel/src/signing_key.rs) naming this seed
#       source and provisioning cause, and prints its `signer`, the
#       fingerprint of the public half of the key the seed derives. The
#       seed is `unsealed`, `provisioned-and-sealed` or `ephemeral`; the
#       cause is `none`, `empty`, `state-changed` or `foreign-slot`.
#   measured-boot-check.sh tamper <payload> <copy>
#       Writes `copy`, the `helios-bootfs` payload with every bit of one
#       byte inside its signed body inverted.
#   measured-boot-check.sh refused <serial-log>
#       Asserts the kernel refused the boot payload on its signature
#       (kernel/src/embedded/init.rs) and settled no key.
set -euo pipefail

settled_marker='compiler-signing key settled'
# The signature check's own error. The sentence around it names where the
# backend found the payload (a Limine module, the riscv initrd).
refusal='payload: bootfs signature does not match the image body'

case "${1:-}" in
  settled)
    log="${2:?serial log}"
    seed="${3:?seed source}"
    cause="${4:?provisioning cause}"
    test "$(grep -acF "$settled_marker" "$log")" -eq 1
    line="$(grep -aF "$settled_marker" "$log")"
    grep -qF "seed=$seed" <<<"$line"
    grep -qF "cause=$cause" <<<"$line"
    grep -ao 'signer=[0-9a-f]\{16\}' <<<"$line" | cut -d= -f2
    ;;
  tamper)
    payload="${2:?payload}"
    copy="${3:?copy}"
    # The signature footer is the last 128 bytes (artifact/src/bootfs.rs)
    # and the body carries the programs, so this offset is inside it.
    offset=4096
    test "$(wc -c <"$payload")" -gt $((offset + 128))
    cp "$payload" "$copy"
    byte="$(dd if="$payload" bs=1 skip="$offset" count=1 status=none | od -An -tu1 | tr -d ' ')"
    printf "\\$(printf '%03o' $((byte ^ 255)))" \
      | dd of="$copy" bs=1 seek="$offset" conv=notrunc status=none
    ! cmp -s "$payload" "$copy"
    ;;
  refused)
    log="${2:?serial log}"
    grep -aF "$refusal" "$log"
    ! grep -aqF "$settled_marker" "$log"
    ;;
  *)
    echo "usage: $0 settled <serial-log> <seed> <cause> | tamper <payload> <copy> | refused <serial-log>" >&2
    exit 2
    ;;
esac

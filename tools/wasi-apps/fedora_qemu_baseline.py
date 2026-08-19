#!/usr/bin/env python3
import json
import os
import platform
import re
import shlex
import shutil
import socket
import subprocess
import tomllib
import time
from pathlib import Path


FEDORA_IMAGE_URLS = {
    "aarch64": (
        "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/aarch64/images/"
        "Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2"
    ),
    "x86_64": (
        "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/"
        "Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2"
    ),
}
QEMU_BINS = {
    "aarch64": "qemu-system-aarch64",
    "x86_64": "qemu-system-x86_64",
}
def default_asset_dir(guest_arch: str) -> Path:
    return Path(f"target/perf-baselines/linux-vm/fedora-{guest_arch}")


def default_accel(guest_arch: str) -> str:
    """Pick the fastest available QEMU accelerator for `guest_arch` on this host."""
    host_system = platform.system()
    host_arch = platform.machine()
    native = (host_arch, guest_arch) in (
        ("arm64", "aarch64"),
        ("aarch64", "aarch64"),
        ("x86_64", "x86_64"),
        ("AMD64", "x86_64"),
    )
    if native and host_system == "Darwin":
        return "hvf"
    if native and host_system == "Linux" and os.access("/dev/kvm", os.R_OK | os.W_OK):
        return "kvm"
    return "tcg"


def machine_and_cpu(guest_arch: str, accel: str) -> tuple[str, str]:
    """QEMU -machine/-cpu values for the Fedora guest."""
    cpu = "host" if accel in ("hvf", "kvm") else "max"
    if guest_arch == "aarch64":
        return f"virt,gic-version=3,accel={accel}", cpu
    if guest_arch == "x86_64":
        return f"q35,accel={accel}", cpu
    raise SystemExit(f"unsupported Fedora guest architecture: {guest_arch}")
DEFAULT_DISK_SIZE = "12G"
DEFAULT_MEMORY = "2G"
DEFAULT_SMP = 4
QUICKJS_SOURCE_URL = "https://github.com/quickjs-ng/quickjs/archive/refs/tags/v0.14.0.tar.gz"
QUICKJS_VERSION = "0.14.0"
QUICKJS_SOURCE_ARCHIVE_NAME = f"quickjs-ng-{QUICKJS_VERSION}.tar.gz"
QUICKJS_POLICY_FILE = "/var/lib/helios-fedora-qemu-bench-quickjs-policy"
SSH_USER = "bench"
REMOTE_ROOT = "/home/bench/helios"
REMOTE_OUT = "/home/bench/out"
REMOTE_QUICKJS_SOURCE_ARCHIVE = f"{REMOTE_ROOT}/sources/quickjs.tar.gz"
REMOTE_WASMTIME_BIN = f"{REMOTE_ROOT}/tools/wasmtime"
REMOTE_WASMTIME_ARCHIVE = f"{REMOTE_ROOT}/sources/wasmtime-linux.tar"
FEDORA_PACKAGES = [
    "python3",
    "bash",
    "dash",
    "coreutils",
    "curl",
    "hyperfine",
    "gcc",
    "make",
    "cmake",
    "tar",
    "gzip",
]


def run(command: list[str], cwd: Path, timeout: int | None = None) -> None:
    subprocess.run(command, cwd=cwd, check=True, timeout=timeout)


def output(command: list[str], cwd: Path, timeout: int | None = None) -> str:
    completed = subprocess.run(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=True,
        timeout=timeout,
    )
    return completed.stdout.strip()


def quickjs_wasm_artifact(repo_root: Path) -> Path:
    manifest_path = repo_root / "tools/wasi-apps/boot-artifacts.toml"
    manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    for artifact in manifest["artifact"]:
        if artifact["command"] == "quickjs":
            return repo_root / artifact["source"]
    raise SystemExit(f"boot artifact manifest does not define quickjs: {manifest_path}")


def wasm_uses_simd(repo_root: Path, path: Path) -> bool:
    if not path.is_file():
        raise SystemExit(f"cannot inspect missing QuickJS wasm artifact for SIMD: {path}")
    text = output(
        ["wasm-tools", "strip", "--all", "--wat", str(path)],
        repo_root,
        timeout=60,
    )
    simd_tokens = (
        "v128.",
        "i8x16.",
        "i16x8.",
        "i32x4.",
        "i64x2.",
        "f32x4.",
        "f64x2.",
    )
    return any(token in text for token in simd_tokens)


# QuickJS measures interpreter/runtime CPU cost, not vector throughput, but the
# benchmark should still look like a real optimized deployment. Helios must run
# a SIMD-capable QuickJS wasm artifact, and Fedora must run the same QuickJS-NG
# source with native SIMD enabled. A scalar QuickJS wasm is benchmark setup
# drift, not a condition to paper over by disabling Linux SIMD.
def quickjs_native_policy(repo_root: Path) -> dict:
    wasm_path = quickjs_wasm_artifact(repo_root)
    uses_simd = wasm_uses_simd(repo_root, wasm_path)
    relative_wasm = wasm_path.relative_to(repo_root)
    if not uses_simd:
        raise SystemExit(
            "QuickJS benchmark requires a SIMD-capable Helios wasm artifact; "
            f"{relative_wasm} contains no wasm SIMD instructions. Rebuild WASI "
            "artifacts with tools/wasi-apps/build.sh or provide a SIMD "
            "QUICKJS_WASM."
        )
    return {
        "id": f"quickjs-{QUICKJS_VERSION}-native-simd-o3",
        "wasm_path": str(relative_wasm),
        "wasm_uses_simd": True,
        "cmake_c_flags_release": "-O3 -DNDEBUG -mcpu=native",
        "baseline_strategy": (
            "built from QuickJS-NG v0.14.0 source with native SIMD enabled "
            "because the Helios WASI QuickJS artifact contains wasm SIMD instructions"
        ),
        "native_simd_policy": (
            "enabled for QuickJS realism; Helios and Fedora both execute SIMD-capable QuickJS"
        ),
    }


def free_tcp_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def resolve_asset_dir(repo_root: Path, asset_dir: Path) -> Path:
    return asset_dir if asset_dir.is_absolute() else repo_root / asset_dir


def resolve_optional_path(repo_root: Path, path: Path | None) -> Path | None:
    if path is None:
        return None
    resolved = path if path.is_absolute() else repo_root / path
    if not resolved.exists():
        raise SystemExit(f"path does not exist: {resolved}")
    return resolved


def copy_path_to_guest(
    repo_root: Path,
    key: Path,
    port: int,
    source: Path,
    remote_path: str,
) -> None:
    remote_parent = shlex.quote(str(Path(remote_path).parent))
    ssh(repo_root, key, port, f"mkdir -p {remote_parent}", timeout=30)
    scp_base = [
        "scp",
        "-r",
        "-i",
        str(key),
        "-P",
        str(port),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
    ]
    target = f"{SSH_USER}@127.0.0.1:{remote_path}"
    if source.is_dir():
        ssh(repo_root, key, port, f"rm -rf {shlex.quote(remote_path)}", timeout=30)
        target = f"{SSH_USER}@127.0.0.1:{Path(remote_path).parent}/"
    run([*scp_base, str(source), target], repo_root)


def resolve_quickjs_source_archive(
    repo_root: Path,
    asset_dir: Path,
    archive: Path | None,
) -> Path | None:
    resolved = resolve_optional_path(repo_root, archive)
    if resolved is None:
        resolved = asset_dir / "sources" / QUICKJS_SOURCE_ARCHIVE_NAME
    if resolved.is_file():
        return resolved
    if archive is None:
        return None
    raise SystemExit(
        "explicit QuickJS native source archive does not exist: "
        f"{resolved}. Expected QuickJS-NG v{QUICKJS_VERSION} source from "
        f"{QUICKJS_SOURCE_URL}."
    )


def ensure_private_key(repo_root: Path, asset_dir: Path) -> Path:
    key = asset_dir / "bench_ed25519"
    if key.is_file():
        return key
    run(["ssh-keygen", "-t", "ed25519", "-N", "", "-f", str(key)], repo_root)
    return key


def proxied_user_data_extras() -> str:
    """cloud-config additions for hosts whose outbound traffic must pass
    an HTTP(S) proxy (e.g. CI containers with TLS-intercepting egress).

    Driven by HELIOS_LINUX_VM_HTTP_PROXY (proxy URL as seen from the
    guest, typically via QEMU user-net's 10.0.2.2 host alias) and
    HELIOS_LINUX_VM_PROXY_CA (host path of the proxy's CA bundle).
    """
    proxy = os.environ.get("HELIOS_LINUX_VM_HTTP_PROXY")
    if not proxy:
        return ""
    no_proxy = "localhost,127.0.0.1,10.0.2.2"
    write_files = [
        (
            "/etc/profile.d/helios-proxy.sh",
            f"export http_proxy={proxy}\n"
            f"export https_proxy={proxy}\n"
            f"export no_proxy={no_proxy}\n",
        ),
    ]
    runcmd = [f"echo 'proxy={proxy}' >> /etc/dnf/dnf.conf"]
    ca_path = os.environ.get("HELIOS_LINUX_VM_PROXY_CA")
    if ca_path:
        ca_content = Path(ca_path).read_text(encoding="utf-8")
        write_files.append(
            ("/etc/pki/ca-trust/source/anchors/helios-proxy-ca.crt", ca_content)
        )
        runcmd.append("update-ca-trust")
    lines = ["write_files:"]
    for path, content in write_files:
        lines.append(f"  - path: {path}")
        lines.append('    permissions: "0644"')
        lines.append("    content: |")
        for content_line in content.splitlines():
            lines.append(f"      {content_line}")
    lines.append("runcmd:")
    for command in runcmd:
        lines.append(f"  - {command}")
    return "\n".join(lines) + "\n"


def render_seed(repo_root: Path, asset_dir: Path, public_key: str) -> Path:
    seed_dir = asset_dir / "seed"
    seed_dir.mkdir(parents=True, exist_ok=True)
    template_dir = repo_root / "tools/wasi-apps/fedora-cloud-init"
    (seed_dir / "meta-data").write_text(
        (template_dir / "meta-data").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    user_data = (template_dir / "user-data").read_text(encoding="utf-8")
    user_data = user_data.replace("{ssh_public_key}", public_key)
    extras = proxied_user_data_extras()
    if extras:
        user_data = user_data.rstrip("\n") + "\n" + extras
    (seed_dir / "user-data").write_text(user_data, encoding="utf-8")
    seed_iso = asset_dir / "cidata.iso"
    tmp_stem = asset_dir / "cidata.tmp"
    tmp_iso = tmp_stem.with_suffix(".tmp.iso")
    if tmp_iso.exists():
        tmp_iso.unlink()
    if shutil.which("hdiutil"):
        run(
            [
                "hdiutil",
                "makehybrid",
                "-quiet",
                "-o",
                str(tmp_stem),
                "-iso",
                "-joliet",
                "-default-volume-name",
                "cidata",
                str(seed_dir),
            ],
            repo_root,
        )
    else:
        iso_tool = next(
            (tool for tool in ("genisoimage", "mkisofs", "xorrisofs") if shutil.which(tool)),
            None,
        )
        if iso_tool is None:
            raise SystemExit(
                "no ISO tool found for the cloud-init seed; install genisoimage "
                "(Linux) or run on macOS (hdiutil)"
            )
        run(
            [
                iso_tool,
                "-quiet",
                "-output",
                str(tmp_iso),
                "-volid",
                "cidata",
                "-joliet",
                "-rock",
                str(seed_dir),
            ],
            repo_root,
        )
    tmp_iso.replace(seed_iso)
    return seed_iso


def download_base_image(repo_root: Path, asset_dir: Path, image_url: str) -> Path:
    image_name = image_url.rstrip("/").rsplit("/", 1)[-1]
    if not image_name:
        raise SystemExit(f"Fedora image URL has no filename: {image_url}")
    base = asset_dir / image_name
    if base.is_file():
        return base
    tmp = base.with_suffix(base.suffix + ".tmp")
    if tmp.exists():
        tmp.unlink()
    run(["curl", "--fail", "--location", "--output", str(tmp), image_url], repo_root)
    tmp.replace(base)
    return base


def ensure_guest_disk(repo_root: Path, base: Path, asset_dir: Path, disk_size: str) -> Path:
    disk = asset_dir / "fedora-bench.qcow2"
    if disk.is_file():
        return disk
    run(
        [
            "qemu-img",
            "create",
            "-f",
            "qcow2",
            "-F",
            "qcow2",
            "-b",
            str(base),
            str(disk),
        ],
        repo_root,
    )
    run(["qemu-img", "resize", str(disk), disk_size], repo_root)
    return disk


def find_qemu_file(qemu_bin: str, filename: str) -> Path:
    qemu_path = shutil.which(qemu_bin) if len(Path(qemu_bin).parts) == 1 else qemu_bin
    candidates = []
    if qemu_path:
        qemu_parent = Path(qemu_path).resolve().parent.parent
        candidates.append(qemu_parent / "share/qemu" / filename)
    candidates.extend(
        [
            Path("/opt/homebrew/share/qemu") / filename,
            Path("/usr/local/share/qemu") / filename,
        ]
    )
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    searched = ", ".join(str(candidate) for candidate in candidates)
    raise SystemExit(f"failed to find QEMU firmware {filename}; searched {searched}")


def ensure_firmware_vars(asset_dir: Path, qemu_bin: str) -> tuple[Path, Path]:
    code = find_qemu_file(qemu_bin, "edk2-aarch64-code.fd")
    vars_template = find_qemu_file(qemu_bin, "edk2-arm-vars.fd")
    vars_path = asset_dir / "edk2-aarch64-vars.fd"
    if not vars_path.is_file():
        shutil.copyfile(vars_template, vars_path)
    return code, vars_path


def ssh_base(key: Path, port: int) -> list[str]:
    return [
        "ssh",
        "-i",
        str(key),
        "-p",
        str(port),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
        f"{SSH_USER}@127.0.0.1",
    ]


def ssh(repo_root: Path, key: Path, port: int, command: str, timeout: int | None = None) -> None:
    run([*ssh_base(key, port), f"bash -lc {shlex.quote(command)}"], repo_root, timeout=timeout)


def ssh_output(
    repo_root: Path,
    key: Path,
    port: int,
    command: str,
    timeout: int | None = None,
) -> str:
    return output([*ssh_base(key, port), f"bash -lc {shlex.quote(command)}"], repo_root, timeout=timeout)


def wait_for_ssh(repo_root: Path, key: Path, port: int, timeout_seconds: int) -> None:
    deadline = time.monotonic() + timeout_seconds
    last_error = "ssh not attempted"
    while time.monotonic() < deadline:
        try:
            ssh(repo_root, key, port, "true", timeout=10)
            return
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
            last_error = str(error)
            time.sleep(2)
    raise SystemExit(f"Fedora QEMU guest did not become reachable over SSH: {last_error}")


def start_vm(
    repo_root: Path,
    asset_dir: Path,
    qemu_bin: str,
    disk: Path,
    seed_iso: Path,
    ssh_port: int,
    memory: str,
    smp: int,
    guest_arch: str,
    accel: str,
) -> subprocess.Popen:
    machine, cpu = machine_and_cpu(guest_arch, accel)
    serial_log = asset_dir / "serial.log"
    # aarch64 virt has no default firmware and uses MMIO virtio transports;
    # x86_64 q35 boots the cloud image through SeaBIOS with PCI transports.
    blk_device = "virtio-blk-device" if guest_arch == "aarch64" else "virtio-blk-pci"
    net_device = "virtio-net-device" if guest_arch == "aarch64" else "virtio-net-pci"
    command = [
        qemu_bin,
        "-machine",
        machine,
        "-cpu",
        cpu,
        "-smp",
        str(smp),
        "-m",
        memory,
        "-display",
        "none",
        "-monitor",
        "none",
        "-serial",
        f"file:{serial_log}",
    ]
    if guest_arch == "aarch64":
        code, vars_path = ensure_firmware_vars(asset_dir, qemu_bin)
        command += [
            "-drive",
            f"if=pflash,format=raw,readonly=on,file={code}",
            "-drive",
            f"if=pflash,format=raw,file={vars_path}",
        ]
    command += [
        "-drive",
        f"if=none,format=qcow2,file={disk},id=rootfs",
        "-device",
        f"{blk_device},drive=rootfs",
        "-drive",
        f"if=none,format=raw,readonly=on,file={seed_iso},id=seed",
        "-device",
        f"{blk_device},drive=seed",
        "-netdev",
        f"user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22",
        "-device",
        f"{net_device},netdev=net0",
    ]
    return subprocess.Popen(command, cwd=repo_root, start_new_session=True)


def stop_vm(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=15)


def shutdown_vm(repo_root: Path, key: Path, port: int, process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    try:
        ssh(repo_root, key, port, "sync; sudo poweroff", timeout=10)
        process.wait(timeout=30)
        return
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
        stop_vm(process)


def ensure_provisioned(
    repo_root: Path,
    key: Path,
    port: int,
    timeout_seconds: int,
    quickjs_policy: dict | None,
    quickjs_source_archive_available: bool,
    require_simd_probe: bool,
) -> None:
    marker = "/var/lib/helios-fedora-qemu-bench-provisioned"
    base_verify_command = " && ".join(
        [
            "python3 --version >/dev/null",
            "bash -c true",
            "dash -c true",
            "cat --version >/dev/null",
            "curl --version >/dev/null",
            "hyperfine --version >/dev/null",
        ]
    )
    verify_parts = [base_verify_command]
    quickjs_version_check = ""
    quickjs_policy_check = ""
    if quickjs_policy is not None:
        quickjs_version_check = f"strings /usr/local/bin/qjs | grep -q '{QUICKJS_VERSION}'"
        quickjs_policy_check = (
            f"test \"$(cat {QUICKJS_POLICY_FILE} 2>/dev/null)\" = {shlex.quote(quickjs_policy['id'])}"
        )
        verify_parts.extend(
            [
                quickjs_version_check,
                quickjs_policy_check,
                "/usr/local/bin/qjs -e 'console.log(\"qjs:ok\")' | grep -q 'qjs:ok'",
            ]
        )
    if require_simd_probe:
        verify_parts.append("/usr/local/bin/helios-simd-lanes | grep -q 'simd-lanes:17'")
    verify_command = " && ".join(
        verify_parts
    )
    if ssh_output(repo_root, key, port, f"({verify_command}) && echo yes || true", timeout=20) == "yes":
        return
    if quickjs_policy is not None and not quickjs_source_archive_available:
        raise SystemExit(
            "Fedora guest QuickJS is not already provisioned with policy "
            f"{quickjs_policy['id']}, and no local QuickJS-NG v{QUICKJS_VERSION} "
            "source archive is staged for offline rebuild. Place the archive "
            f"under the Fedora VM asset directory as sources/{QUICKJS_SOURCE_ARCHIVE_NAME} "
            "or pass --quickjs-source-archive."
        )
    package_list = " ".join(shlex.quote(package) for package in FEDORA_PACKAGES)
    command_parts = [f"({base_verify_command}) || sudo dnf install -y {package_list}"]
    if quickjs_policy is not None:
        quickjs_install = " && ".join(
            [
                "quickjs_work=$(mktemp -d)",
                f"test -f {shlex.quote(REMOTE_QUICKJS_SOURCE_ARCHIVE)}",
                f"tar -C \"$quickjs_work\" -xf {shlex.quote(REMOTE_QUICKJS_SOURCE_ARCHIVE)}",
                f"quickjs_src=\"$quickjs_work/quickjs-{QUICKJS_VERSION}\"",
                "cmake -S \"$quickjs_src\" -B \"$quickjs_src/build\" "
                "-DCMAKE_BUILD_TYPE=Release "
                f"-DCMAKE_C_FLAGS_RELEASE={shlex.quote(quickjs_policy['cmake_c_flags_release'])}",
                "cmake --build \"$quickjs_src/build\" --target qjs_exe -j\"$(nproc)\"",
                "sudo install -m 0755 \"$quickjs_src/build/qjs\" /usr/local/bin/qjs",
                f"printf '%s\\n' {shlex.quote(quickjs_policy['id'])} | sudo tee {QUICKJS_POLICY_FILE} >/dev/null",
                "rm -rf \"$quickjs_work\"",
            ]
        )
        command_parts.append(
            f"(({quickjs_version_check} && {quickjs_policy_check}) || ({quickjs_install}))"
        )
    if require_simd_probe:
        simd_install = " && ".join(
            [
                "test -f /home/bench/helios/fedora-guest-tools/simd_lanes.c",
                "gcc -O3 -mcpu=native /home/bench/helios/fedora-guest-tools/simd_lanes.c -o /tmp/helios-simd-lanes",
                "sudo install -m 0755 /tmp/helios-simd-lanes /usr/local/bin/helios-simd-lanes",
                "rm -f /tmp/helios-simd-lanes",
            ]
        )
        command_parts.append(f"test -x /usr/local/bin/helios-simd-lanes || ({simd_install})")
    command_parts.extend([verify_command, f"sudo touch {marker}"])
    command = " && ".join(command_parts)
    ssh(repo_root, key, port, command, timeout=timeout_seconds)


def wasmtime_guest_paths(repo_root: Path, workloads: list[dict]) -> list[Path]:
    paths: dict[str, Path] = {}
    for workload in workloads:
        profile = workload.get("wasmtime_profile")
        if not profile:
            continue
        wasm_path = profile.get("wasm_path")
        if not wasm_path:
            raise SystemExit(f"workload {workload['name']} wasmtime_profile is missing wasm_path")
        profile_paths = [wasm_path, *profile.get("guest_paths", [])]
        for value in profile.get("wasmtime_flags", []):
            profile_paths.extend(
                match.group(1) for match in re.finditer(r"\{repo_root\}/([^:\s]+)", value)
            )
        for relative in profile_paths:
            path = repo_root / relative
            if not path.exists():
                raise SystemExit(f"Wasmtime Linux baseline artifact is missing: {path}")
            paths[str(path.relative_to(repo_root))] = path
    return [paths[key] for key in sorted(paths)]


def copy_guest_files(
    repo_root: Path,
    key: Path,
    port: int,
    quickjs_source_archive: Path | None,
    wasmtime_linux_bin: Path | None,
    wasmtime_linux_archive: Path | None,
    wasmtime_workloads: list[dict],
) -> None:
    ssh(
        repo_root,
        key,
        port,
        f"rm -rf {shlex.quote(REMOTE_ROOT)} {shlex.quote(REMOTE_OUT)} && "
        f"mkdir -p {shlex.quote(REMOTE_ROOT)}/tools/wasi-apps "
        f"{shlex.quote(REMOTE_ROOT)}/fedora-guest-tools "
        f"{shlex.quote(REMOTE_ROOT)}/sources "
        f"{shlex.quote(REMOTE_ROOT)}/artifacts "
        f"{shlex.quote(REMOTE_ROOT)}/tools {shlex.quote(REMOTE_OUT)}",
        timeout=30,
    )
    scp_base = [
        "scp",
        "-i",
        str(key),
        "-P",
        str(port),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
    ]
    files = [
        repo_root / "tools/wasi-apps/workloads.json",
        repo_root / "tools/wasi-apps/linux-workload-runner.py",
        repo_root / "tools/wasi-apps/linux_tcp_throughput_client.py",
    ]
    run([*scp_base, *(str(path) for path in files), f"{SSH_USER}@127.0.0.1:{REMOTE_ROOT}/tools/wasi-apps/"], repo_root)
    guest_tools = [repo_root / "tools/wasi-apps/fedora-guest-tools/simd_lanes.c"]
    run([*scp_base, *(str(path) for path in guest_tools), f"{SSH_USER}@127.0.0.1:{REMOTE_ROOT}/fedora-guest-tools/"], repo_root)
    if quickjs_source_archive is not None:
        run(
            [
                *scp_base,
                str(quickjs_source_archive),
                f"{SSH_USER}@127.0.0.1:{REMOTE_QUICKJS_SOURCE_ARCHIVE}",
            ],
            repo_root,
        )
    if wasmtime_linux_bin is not None:
        copy_path_to_guest(repo_root, key, port, wasmtime_linux_bin, REMOTE_WASMTIME_BIN)
        ssh(repo_root, key, port, f"chmod 0755 {shlex.quote(REMOTE_WASMTIME_BIN)}", timeout=30)
    if wasmtime_linux_archive is not None:
        copy_path_to_guest(
            repo_root,
            key,
            port,
            wasmtime_linux_archive,
            REMOTE_WASMTIME_ARCHIVE,
        )
    for path in wasmtime_guest_paths(repo_root, wasmtime_workloads):
        remote_path = f"{REMOTE_ROOT}/{path.relative_to(repo_root)}"
        copy_path_to_guest(repo_root, key, port, path, remote_path)


def ensure_wasmtime_linux(
    repo_root: Path,
    key: Path,
    port: int,
    wasmtime_linux_bin: Path | None,
    wasmtime_linux_archive: Path | None,
) -> str:
    if wasmtime_linux_bin is not None:
        version = ssh_output(repo_root, key, port, f"{shlex.quote(REMOTE_WASMTIME_BIN)} --version", timeout=20)
        return f"{REMOTE_WASMTIME_BIN} ({version})"
    if wasmtime_linux_archive is not None:
        install = " && ".join(
            [
                "rm -rf /home/bench/helios/tools/wasmtime-extract",
                "mkdir -p /home/bench/helios/tools/wasmtime-extract",
                f"tar -C /home/bench/helios/tools/wasmtime-extract -xf {shlex.quote(REMOTE_WASMTIME_ARCHIVE)}",
                "wasmtime_found=$(find /home/bench/helios/tools/wasmtime-extract -type f -name wasmtime -perm -u+x | head -n 1)",
                "test -n \"$wasmtime_found\"",
                f"cp \"$wasmtime_found\" {shlex.quote(REMOTE_WASMTIME_BIN)}",
                f"chmod 0755 {shlex.quote(REMOTE_WASMTIME_BIN)}",
            ]
        )
        ssh(repo_root, key, port, install, timeout=60)
        version = ssh_output(repo_root, key, port, f"{shlex.quote(REMOTE_WASMTIME_BIN)} --version", timeout=20)
        return f"{REMOTE_WASMTIME_BIN} ({version})"
    version = ssh_output(repo_root, key, port, "wasmtime --version 2>/dev/null || true", timeout=20)
    if not version:
        raise SystemExit(
            "Wasmtime-on-Linux baseline requested, but the Fedora guest has no "
            "wasmtime binary. Pass --wasmtime-linux-bin with a Linux wasmtime "
            "executable matching the guest architecture, or "
            "--wasmtime-linux-archive with a pre-staged Linux Wasmtime tar archive."
        )
    return f"wasmtime ({version})"


def linux_cpu_features(
    repo_root: Path,
    key: Path,
    port: int,
    quickjs_policy: dict | None,
    require_simd_probe: bool,
) -> dict:
    cpuinfo = ssh_output(repo_root, key, port, "cat /proc/cpuinfo", timeout=20)
    features = []
    for line in cpuinfo.splitlines():
        # aarch64 reports "Features", x86_64 reports "flags".
        if line.startswith(("Features", "flags")):
            _, _, value = line.partition(":")
            features = value.split()
            break
    result = {
        "cpu_features": features,
        "simd": "asimd" in features or "sse2" in features,
        "quickjs_required": quickjs_policy is not None,
        "native_simd_probe_required": require_simd_probe,
    }
    if quickjs_policy is not None:
        qjs_version = ssh_output(
            repo_root,
            key,
            port,
            f"strings /usr/local/bin/qjs | grep '{QUICKJS_VERSION}' | head -n 1",
            timeout=20,
        )
        result.update(
            {
                "quickjs_native_version": qjs_version,
                "quickjs_wasm_version": f"QuickJS-ng version {QUICKJS_VERSION}",
                "quickjs_wasm_path": quickjs_policy["wasm_path"],
                "quickjs_wasm_uses_simd": quickjs_policy["wasm_uses_simd"],
                "quickjs_native_policy_id": quickjs_policy["id"],
                "quickjs_native_c_flags_release": quickjs_policy["cmake_c_flags_release"],
                "quickjs_baseline_strategy": quickjs_policy["baseline_strategy"],
                "quickjs_source_url": QUICKJS_SOURCE_URL,
                "quickjs_native_simd_policy": quickjs_policy["native_simd_policy"],
            }
        )
    if require_simd_probe:
        result["native_simd_probe"] = ssh_output(
            repo_root, key, port, "/usr/local/bin/helios-simd-lanes", timeout=20
        )
    return result


def hyperfine_command(
    iterations: int,
    workloads: list[dict],
    host_http_url: str | None,
    host_tcp_host: str | None,
    host_tcp_port: int | None,
    output_name: str,
    mode: str,
    wasmtime_bin: str | None = None,
) -> str:
    command = [
        "hyperfine",
        "--runs",
        str(iterations),
        "--export-json",
        f"{REMOTE_OUT}/{output_name}",
    ]
    for workload in workloads:
        runner = [
            "python3",
            f"{REMOTE_ROOT}/tools/wasi-apps/linux-workload-runner.py",
            "--manifest",
            f"{REMOTE_ROOT}/tools/wasi-apps/workloads.json",
            "--workload",
            workload["name"],
            "--mode",
            mode,
        ]
        if wasmtime_bin is not None:
            runner.extend(["--wasmtime-bin", wasmtime_bin])
        if host_http_url:
            runner.extend(["--host-http-url", host_http_url])
        if host_tcp_host and host_tcp_port is not None:
            runner.extend(["--host-tcp-host", host_tcp_host, "--host-tcp-port", str(host_tcp_port)])
        command.extend(["--command-name", workload["name"], shlex.join(runner)])
    return shlex.join(command)


def copy_hyperfine_json(repo_root: Path, key: Path, port: int, remote_name: str, destination: Path) -> None:
    scp_base = [
        "scp",
        "-i",
        str(key),
        "-P",
        str(port),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
    ]
    run([*scp_base, f"{SSH_USER}@127.0.0.1:{REMOTE_OUT}/{remote_name}", str(destination)], repo_root)


def workload_uses_placeholder(workload: dict, placeholder: str) -> bool:
    values = [workload.get("command", ""), workload.get("program", ""), *workload.get("args", [])]
    return any(placeholder in value for value in values)


def run_fedora_qemu_linux(
    repo_root: Path,
    out_dir: Path,
    iterations: int,
    workloads: list[dict],
    image_url: str,
    asset_dir: Path,
    qemu_bin: str,
    ssh_port: int | None,
    memory: str,
    smp: int,
    disk_size: str,
    setup_timeout_seconds: int,
    host_http_url: str | None,
    host_tcp_host: str | None,
    host_tcp_port: int | None,
    quickjs_source_archive: Path | None = None,
    wasmtime_linux_bin: Path | None = None,
    wasmtime_linux_archive: Path | None = None,
    guest_arch: str = "aarch64",
    accel: str | None = None,
) -> tuple[Path | None, Path | None, dict]:
    accel = accel or default_accel(guest_arch)
    machine, cpu = machine_and_cpu(guest_arch, accel)
    native_workloads = [
        workload for workload in workloads if workload["runner"] in ("shell", "program")
    ]
    wasmtime_workloads = [workload for workload in workloads if workload.get("wasmtime_profile")]
    asset_dir = resolve_asset_dir(repo_root, asset_dir)
    wasmtime_linux_bin = resolve_optional_path(repo_root, wasmtime_linux_bin)
    wasmtime_linux_archive = resolve_optional_path(repo_root, wasmtime_linux_archive)
    quickjs_required = any(
        workload_uses_placeholder(workload, "{quickjs}") for workload in native_workloads
    )
    simd_probe_required = any(
        workload_uses_placeholder(workload, "{simd_lanes}") for workload in native_workloads
    )
    quickjs_policy = quickjs_native_policy(repo_root) if quickjs_required else None
    quickjs_source_archive = (
        resolve_quickjs_source_archive(repo_root, asset_dir, quickjs_source_archive)
        if quickjs_required
        else None
    )
    asset_dir.mkdir(parents=True, exist_ok=True)
    key = ensure_private_key(repo_root, asset_dir)
    public_key = key.with_suffix(key.suffix + ".pub").read_text(encoding="utf-8").strip()
    seed_iso = render_seed(repo_root, asset_dir, public_key)
    base = download_base_image(repo_root, asset_dir, image_url)
    disk = ensure_guest_disk(repo_root, base, asset_dir, disk_size)
    blk_device = "virtio-blk-device" if guest_arch == "aarch64" else "virtio-blk-pci"
    net_device = "virtio-net-device" if guest_arch == "aarch64" else "virtio-net-pci"
    provenance = {
        "kind": f"fedora-qemu-{guest_arch}-{accel}",
        "fedora_image_url": image_url,
        "fedora_base_image": str(base),
        "fedora_guest_disk": str(disk),
        "qemu_bin": qemu_bin,
        "qemu_machine": machine,
        "qemu_cpu": cpu,
        "qemu_smp": smp,
        "qemu_memory": memory,
        "network": f"{net_device} over QEMU user net; workloads connect to host through 10.0.2.2",
        "block": blk_device,
        "quickjs_required": quickjs_required,
        "native_simd_probe_required": simd_probe_required,
        "wasmtime_linux_required": bool(wasmtime_workloads),
        "wasmtime_linux_workloads": [workload["name"] for workload in wasmtime_workloads],
    }
    if quickjs_source_archive is not None:
        provenance["quickjs_source_archive"] = str(quickjs_source_archive)
    elif quickjs_required:
        provenance["quickjs_source_archive"] = "not-staged; using pre-provisioned guest qjs"
    if wasmtime_linux_bin is not None:
        provenance["wasmtime_linux_bin"] = str(wasmtime_linux_bin)
    if wasmtime_linux_archive is not None:
        provenance["wasmtime_linux_archive"] = str(wasmtime_linux_archive)
    if not native_workloads and not wasmtime_workloads:
        return None, None, provenance
    port = ssh_port or free_tcp_port()
    process = start_vm(
        repo_root, asset_dir, qemu_bin, disk, seed_iso, port, memory, smp, guest_arch, accel
    )
    try:
        wait_for_ssh(repo_root, key, port, setup_timeout_seconds)
        copy_guest_files(
            repo_root,
            key,
            port,
            quickjs_source_archive,
            wasmtime_linux_bin,
            wasmtime_linux_archive,
            wasmtime_workloads,
        )
        ensure_provisioned(
            repo_root,
            key,
            port,
            setup_timeout_seconds,
            quickjs_policy,
            quickjs_source_archive is not None,
            simd_probe_required,
        )
        provenance.update(
            linux_cpu_features(repo_root, key, port, quickjs_policy, simd_probe_required)
        )
        wasmtime_bin = None
        if wasmtime_workloads:
            provenance["wasmtime_linux"] = ensure_wasmtime_linux(
                repo_root,
                key,
                port,
                wasmtime_linux_bin,
                wasmtime_linux_archive,
            )
            wasmtime_bin = REMOTE_WASMTIME_BIN if (
                wasmtime_linux_bin is not None or wasmtime_linux_archive is not None
            ) else "wasmtime"
        hyperfine_json = None
        if native_workloads:
            ssh(
                repo_root,
                key,
                port,
                hyperfine_command(
                    iterations,
                    native_workloads,
                    host_http_url,
                    host_tcp_host,
                    host_tcp_port,
                    "linux-hyperfine.json",
                    "native",
                ),
                timeout=setup_timeout_seconds,
            )
            hyperfine_json = out_dir / "linux-hyperfine.json"
            copy_hyperfine_json(repo_root, key, port, "linux-hyperfine.json", hyperfine_json)
        wasmtime_hyperfine_json = None
        if wasmtime_workloads:
            ssh(
                repo_root,
                key,
                port,
                hyperfine_command(
                    iterations,
                    wasmtime_workloads,
                    host_http_url,
                    host_tcp_host,
                    host_tcp_port,
                    "wasmtime-linux-hyperfine.json",
                    "wasmtime",
                    wasmtime_bin,
                ),
                timeout=setup_timeout_seconds,
            )
            wasmtime_hyperfine_json = out_dir / "wasmtime-linux-hyperfine.json"
            copy_hyperfine_json(
                repo_root,
                key,
                port,
                "wasmtime-linux-hyperfine.json",
                wasmtime_hyperfine_json,
            )
        return hyperfine_json, wasmtime_hyperfine_json, provenance
    finally:
        shutdown_vm(repo_root, key, port, process)

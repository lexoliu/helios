#!/usr/bin/env python3
import json
import os
import shlex
import shutil
import socket
import subprocess
import time
from pathlib import Path


DEFAULT_FEDORA_IMAGE_URL = (
    "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/aarch64/images/"
    "Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2"
)
DEFAULT_ASSET_DIR = Path("target/perf-baselines/linux-vm/fedora-aarch64")
DEFAULT_DISK_SIZE = "12G"
DEFAULT_MEMORY = "2G"
DEFAULT_SMP = 4
QUICKJS_SOURCE_URL = "https://github.com/quickjs-ng/quickjs/archive/refs/tags/v0.14.0.tar.gz"
QUICKJS_VERSION = "0.14.0"
SSH_USER = "bench"
REMOTE_ROOT = "/home/bench/helios"
REMOTE_OUT = "/home/bench/out"
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


def free_tcp_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def resolve_asset_dir(repo_root: Path, asset_dir: Path) -> Path:
    return asset_dir if asset_dir.is_absolute() else repo_root / asset_dir


def ensure_private_key(repo_root: Path, asset_dir: Path) -> Path:
    key = asset_dir / "bench_ed25519"
    if key.is_file():
        return key
    run(["ssh-keygen", "-t", "ed25519", "-N", "", "-f", str(key)], repo_root)
    return key


def render_seed(repo_root: Path, asset_dir: Path, public_key: str) -> Path:
    seed_dir = asset_dir / "seed"
    seed_dir.mkdir(parents=True, exist_ok=True)
    template_dir = repo_root / "tools/wasi-apps/fedora-cloud-init"
    (seed_dir / "meta-data").write_text(
        (template_dir / "meta-data").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    user_data = (template_dir / "user-data").read_text(encoding="utf-8")
    (seed_dir / "user-data").write_text(
        user_data.replace("{ssh_public_key}", public_key),
        encoding="utf-8",
    )
    seed_iso = asset_dir / "cidata.iso"
    tmp_stem = asset_dir / "cidata.tmp"
    tmp_iso = tmp_stem.with_suffix(".tmp.iso")
    if tmp_iso.exists():
        tmp_iso.unlink()
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
) -> subprocess.Popen:
    code, vars_path = ensure_firmware_vars(asset_dir, qemu_bin)
    serial_log = asset_dir / "serial.log"
    command = [
        qemu_bin,
        "-machine",
        "virt,gic-version=3,accel=hvf",
        "-cpu",
        "host",
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
        "-drive",
        f"if=pflash,format=raw,readonly=on,file={code}",
        "-drive",
        f"if=pflash,format=raw,file={vars_path}",
        "-drive",
        f"if=none,format=qcow2,file={disk},id=rootfs",
        "-device",
        "virtio-blk-device,drive=rootfs",
        "-drive",
        f"if=none,format=raw,readonly=on,file={seed_iso},id=seed",
        "-device",
        "virtio-blk-device,drive=seed",
        "-netdev",
        f"user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22",
        "-device",
        "virtio-net-device,netdev=net0",
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


def ensure_provisioned(repo_root: Path, key: Path, port: int, timeout_seconds: int) -> None:
    marker = "/var/lib/helios-fedora-qemu-bench-provisioned"
    quickjs_version_check = f"strings /usr/local/bin/qjs | grep -q '{QUICKJS_VERSION}'"
    verify_command = " && ".join(
        [
            quickjs_version_check,
            "/usr/local/bin/qjs -e 'console.log(\"qjs:ok\")' | grep -q 'qjs:ok'",
            "/usr/local/bin/helios-simd-lanes | grep -q 'simd-lanes:17'",
            "hyperfine --version >/dev/null",
        ]
    )
    if ssh_output(repo_root, key, port, f"test -f {marker} && ({verify_command}) && echo yes || true", timeout=20) == "yes":
        return
    package_list = " ".join(shlex.quote(package) for package in FEDORA_PACKAGES)
    quickjs_install = " && ".join(
        [
            "quickjs_work=$(mktemp -d)",
            f"curl --fail --location --output \"$quickjs_work/quickjs.tar.gz\" {shlex.quote(QUICKJS_SOURCE_URL)}",
            "tar -C \"$quickjs_work\" -xf \"$quickjs_work/quickjs.tar.gz\"",
            f"quickjs_src=\"$quickjs_work/quickjs-{QUICKJS_VERSION}\"",
            "cmake -S \"$quickjs_src\" -B \"$quickjs_src/build\" -DCMAKE_BUILD_TYPE=Release -DCMAKE_C_FLAGS_RELEASE='-O2 -DNDEBUG -march=armv8-a+nosimd'",
            "cmake --build \"$quickjs_src/build\" --target qjs_exe -j\"$(nproc)\"",
            "sudo install -m 0755 \"$quickjs_src/build/qjs\" /usr/local/bin/qjs",
            "rm -rf \"$quickjs_work\"",
        ]
    )
    simd_install = " && ".join(
        [
            "test -f /home/bench/helios/fedora-guest-tools/simd_lanes.c",
            "gcc -O3 -mcpu=native /home/bench/helios/fedora-guest-tools/simd_lanes.c -o /tmp/helios-simd-lanes",
            "sudo install -m 0755 /tmp/helios-simd-lanes /usr/local/bin/helios-simd-lanes",
            "rm -f /tmp/helios-simd-lanes",
        ]
    )
    command = " && ".join(
        [
            f"sudo dnf install -y {package_list}",
            f"({quickjs_version_check} || ({quickjs_install}))",
            f"test -x /usr/local/bin/helios-simd-lanes || ({simd_install})",
            verify_command,
            f"sudo touch {marker}",
        ]
    )
    ssh(repo_root, key, port, command, timeout=timeout_seconds)


def copy_guest_files(repo_root: Path, key: Path, port: int) -> None:
    ssh(
        repo_root,
        key,
        port,
        f"rm -rf {shlex.quote(REMOTE_ROOT)} {shlex.quote(REMOTE_OUT)} && "
        f"mkdir -p {shlex.quote(REMOTE_ROOT)}/tools/wasi-apps "
        f"{shlex.quote(REMOTE_ROOT)}/fedora-guest-tools {shlex.quote(REMOTE_OUT)}",
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


def linux_cpu_features(repo_root: Path, key: Path, port: int) -> dict:
    cpuinfo = ssh_output(repo_root, key, port, "cat /proc/cpuinfo", timeout=20)
    features = []
    for line in cpuinfo.splitlines():
        if line.startswith("Features"):
            _, _, value = line.partition(":")
            features = value.split()
            break
    qjs_version = ssh_output(
        repo_root,
        key,
        port,
        f"strings /usr/local/bin/qjs | grep '{QUICKJS_VERSION}' | head -n 1",
        timeout=20,
    )
    simd_output = ssh_output(repo_root, key, port, "/usr/local/bin/helios-simd-lanes", timeout=20)
    return {
        "aarch64_features": features,
        "asimd": "asimd" in features,
        "quickjs_native_version": qjs_version,
        "quickjs_wasm_version": f"QuickJS-ng version {QUICKJS_VERSION}",
        "quickjs_baseline_strategy": "built from QuickJS-NG v0.14.0 source with -march=armv8-a+nosimd because the Helios WASI QuickJS artifact does not contain wasm SIMD instructions",
        "quickjs_source_url": QUICKJS_SOURCE_URL,
        "quickjs_native_simd_policy": "disabled for QuickJS fairness; native SIMD is measured only by the separate simd-lanes workload",
        "native_simd_probe": simd_output,
    }


def hyperfine_command(
    iterations: int,
    workloads: list[dict],
    host_http_url: str | None,
    host_tcp_host: str | None,
    host_tcp_port: int | None,
) -> str:
    command = [
        "hyperfine",
        "--runs",
        str(iterations),
        "--export-json",
        f"{REMOTE_OUT}/linux-hyperfine.json",
    ]
    for workload in workloads:
        runner = [
            "python3",
            f"{REMOTE_ROOT}/tools/wasi-apps/linux-workload-runner.py",
            "--manifest",
            f"{REMOTE_ROOT}/tools/wasi-apps/workloads.json",
            "--workload",
            workload["name"],
        ]
        if host_http_url:
            runner.extend(["--host-http-url", host_http_url])
        if host_tcp_host and host_tcp_port is not None:
            runner.extend(["--host-tcp-host", host_tcp_host, "--host-tcp-port", str(host_tcp_port)])
        command.extend(["--command-name", workload["name"], shlex.join(runner)])
    return shlex.join(command)


def copy_hyperfine_json(repo_root: Path, key: Path, port: int, destination: Path) -> None:
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
    run([*scp_base, f"{SSH_USER}@127.0.0.1:{REMOTE_OUT}/linux-hyperfine.json", str(destination)], repo_root)


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
) -> tuple[Path | None, dict]:
    native_workloads = [
        workload for workload in workloads if workload["runner"] in ("shell", "program")
    ]
    asset_dir = resolve_asset_dir(repo_root, asset_dir)
    asset_dir.mkdir(parents=True, exist_ok=True)
    key = ensure_private_key(repo_root, asset_dir)
    public_key = key.with_suffix(key.suffix + ".pub").read_text(encoding="utf-8").strip()
    seed_iso = render_seed(repo_root, asset_dir, public_key)
    base = download_base_image(repo_root, asset_dir, image_url)
    disk = ensure_guest_disk(repo_root, base, asset_dir, disk_size)
    provenance = {
        "kind": "fedora-qemu-aarch64-hvf",
        "fedora_image_url": image_url,
        "fedora_base_image": str(base),
        "fedora_guest_disk": str(disk),
        "qemu_bin": qemu_bin,
        "qemu_machine": "virt,gic-version=3,accel=hvf",
        "qemu_cpu": "host",
        "qemu_smp": smp,
        "qemu_memory": memory,
        "network": "virtio-net-device over QEMU user net; workloads connect to host through 10.0.2.2",
        "block": "virtio-blk-device",
    }
    if not native_workloads:
        return None, provenance
    port = ssh_port or free_tcp_port()
    process = start_vm(repo_root, asset_dir, qemu_bin, disk, seed_iso, port, memory, smp)
    try:
        wait_for_ssh(repo_root, key, port, setup_timeout_seconds)
        copy_guest_files(repo_root, key, port)
        ensure_provisioned(repo_root, key, port, setup_timeout_seconds)
        provenance.update(linux_cpu_features(repo_root, key, port))
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
            ),
            timeout=setup_timeout_seconds,
        )
        hyperfine_json = out_dir / "linux-hyperfine.json"
        copy_hyperfine_json(repo_root, key, port, hyperfine_json)
        return hyperfine_json, provenance
    finally:
        shutdown_vm(repo_root, key, port, process)

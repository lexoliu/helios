#!/usr/bin/env python3
import json
import os
import shlex
import shutil
import socket
import subprocess
import tomllib
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
QUICKJS_SOURCE_ARCHIVE_NAME = f"quickjs-ng-{QUICKJS_VERSION}.tar.gz"
QUICKJS_POLICY_FILE = "/var/lib/helios-fedora-qemu-bench-quickjs-policy"
SSH_USER = "bench"
REMOTE_ROOT = "/home/bench/helios"
REMOTE_OUT = "/home/bench/out"
REMOTE_QUICKJS_SOURCE_ARCHIVE = f"{REMOTE_ROOT}/sources/quickjs.tar.gz"
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


# QuickJS measures interpreter/runtime CPU cost, not vector throughput. Keep the
# Fedora native build in SIMD lockstep with the Helios wasm artifact: if the
# stripped wasm code has no SIMD opcodes, the Linux baseline is built with SIMD
# and GCC auto-vectorization disabled. The separate simd-lanes workload is the
# explicit vector-capability check, so quickjs-loop cannot accidentally become
# "Fedora native SIMD vs Helios scalar wasm" evidence.
def quickjs_native_policy(repo_root: Path) -> dict:
    wasm_path = quickjs_wasm_artifact(repo_root)
    uses_simd = wasm_uses_simd(repo_root, wasm_path)
    relative_wasm = wasm_path.relative_to(repo_root)
    if uses_simd:
        return {
            "id": f"quickjs-{QUICKJS_VERSION}-native-simd",
            "wasm_path": str(relative_wasm),
            "wasm_uses_simd": True,
            "cmake_c_flags_release": "-O2 -DNDEBUG -mcpu=native",
            "baseline_strategy": (
                "built from QuickJS-NG v0.14.0 source with native SIMD enabled "
                "because the Helios WASI QuickJS artifact contains wasm SIMD instructions"
            ),
            "native_simd_policy": (
                "enabled for QuickJS fairness; Helios and Fedora both execute SIMD-capable QuickJS"
            ),
        }
    return {
        "id": f"quickjs-{QUICKJS_VERSION}-native-nosimd-novec",
        "wasm_path": str(relative_wasm),
        "wasm_uses_simd": False,
        "cmake_c_flags_release": (
            "-O2 -DNDEBUG -march=armv8-a+nosimd "
            "-fno-tree-vectorize -fno-tree-slp-vectorize"
        ),
        "baseline_strategy": (
            "built from QuickJS-NG v0.14.0 source with native vectorization disabled "
            "because the Helios WASI QuickJS artifact does not contain wasm SIMD instructions"
        ),
        "native_simd_policy": (
            "disabled for QuickJS fairness; native SIMD is measured only by the separate simd-lanes workload"
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
    return path if path.is_absolute() else repo_root / path


def resolve_quickjs_source_archive(
    repo_root: Path,
    asset_dir: Path,
    archive: Path | None,
) -> Path:
    resolved = resolve_optional_path(repo_root, archive)
    if resolved is None:
        resolved = asset_dir / "sources" / QUICKJS_SOURCE_ARCHIVE_NAME
    if resolved.is_file():
        return resolved
    raise SystemExit(
        "QuickJS native baseline requires a pre-staged source archive for offline "
        f"Fedora provisioning: {resolved}. Expected QuickJS-NG v{QUICKJS_VERSION} "
        f"source from {QUICKJS_SOURCE_URL}."
    )


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


def ensure_provisioned(
    repo_root: Path,
    key: Path,
    port: int,
    timeout_seconds: int,
    quickjs_policy: dict | None,
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


def copy_guest_files(
    repo_root: Path,
    key: Path,
    port: int,
    quickjs_source_archive: Path | None,
) -> None:
    ssh(
        repo_root,
        key,
        port,
        f"rm -rf {shlex.quote(REMOTE_ROOT)} {shlex.quote(REMOTE_OUT)} && "
        f"mkdir -p {shlex.quote(REMOTE_ROOT)}/tools/wasi-apps "
        f"{shlex.quote(REMOTE_ROOT)}/fedora-guest-tools "
        f"{shlex.quote(REMOTE_ROOT)}/sources {shlex.quote(REMOTE_OUT)}",
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
        if line.startswith("Features"):
            _, _, value = line.partition(":")
            features = value.split()
            break
    result = {
        "aarch64_features": features,
        "asimd": "asimd" in features,
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
) -> tuple[Path | None, dict]:
    native_workloads = [
        workload for workload in workloads if workload["runner"] in ("shell", "program")
    ]
    asset_dir = resolve_asset_dir(repo_root, asset_dir)
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
        "quickjs_required": quickjs_required,
        "native_simd_probe_required": simd_probe_required,
    }
    if quickjs_source_archive is not None:
        provenance["quickjs_source_archive"] = str(quickjs_source_archive)
    if not native_workloads:
        return None, provenance
    port = ssh_port or free_tcp_port()
    process = start_vm(repo_root, asset_dir, qemu_bin, disk, seed_iso, port, memory, smp)
    try:
        wait_for_ssh(repo_root, key, port, setup_timeout_seconds)
        copy_guest_files(repo_root, key, port, quickjs_source_archive)
        ensure_provisioned(
            repo_root,
            key,
            port,
            setup_timeout_seconds,
            quickjs_policy,
            simd_probe_required,
        )
        provenance.update(
            linux_cpu_features(repo_root, key, port, quickjs_policy, simd_probe_required)
        )
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

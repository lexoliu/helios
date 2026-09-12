"""The baseline image of a paired run: a second Helios checkout whose
guest is built and timed beside the candidate's, in the same job.

A shared runner does not pin the CPU model, so two runs of one lane are
two machines as often as they are one machine twice: run 33990628290
reported every workload 20-40% faster than the `dev` run it was compared
against, including workloads its change could not touch (#173). The
answer is not a quieter runner but a second column taken on the same
machine.

The baseline checkout supplies a guest and the tooling that guest is
built and booted by; the scheduling harness stays the candidate's. One
harness times both images — this checkout's
`tools/wasi-apps/workload-bench.sh`, its workload manifest, its budgets —
and an image is selected by `HELIOS_WORKSPACE_ROOT`, the checkout the
inspector resolves the guest against. But the `helios-inspector` and
`helios-cli` a side runs are compiled from that side's own ref into its
own `target/release`: the inspector and the guest's debugger speak
helios-inspector-protocol, and a record added to it between the two refs
is unanswerable by the other side's tooling — run 34551261487's baseline
boot died in the readiness probe with `DeserializeUnexpectedEnd` under
the candidate's inspector (#356). A baseline whose own tooling does not
build fails the run naming its checkout; there is no fallback to the
candidate's.

Shared, and therefore unable to explain a difference between the columns:

- the host, its CPU model, its load and its thermal state;
- the QEMU release, the accelerator, the vCPU count and the memory;
- the network backend and host HTTP, TCP and echo server implementations
  and payloads; each guest boot gets independent listeners, except for
  explicit diagnostic reuse of peer connection state;
- the harness: the benchmark script that orders and budgets the boots,
  and the workload manifest, read from the candidate checkout for both;
- everything under `artifacts/` that `tools/wasi-apps/build.sh` stages
  (the CPython root, the WASI tools, the WASIX programs), linked into the
  baseline worktree entry by entry rather than copied;
- the vendored Wasmtime checkout, reached by both kernels through the
  one `../wasmtime` path: the baseline checkout is laid out beside the
  candidate's, so the workspace path dependency resolves to the same
  absolute directory. Cargo hashes a path dependency outside the
  workspace by that absolute path, and a baseline that reached the
  checkout through a link of its own compiled every Wasmtime crate
  under a different crate hash — every symbol the kernel profile names
  through Wasmtime then missed, and the pair timed a profile-guided
  candidate against a 94%-unprofiled baseline (#359);
- the fetched kernel profile store (`target/profiles`, docs/pgo.md),
  linked into the worktree so that both x86-64 release kernels compile
  against the one profile in force when the run started (#321).

Per side, and therefore what the comparison measures: the kernel image,
the bootfs it carries (the compiler plugin included), the guest programs
its prebuild signs and the `helios-inspector`/`helios-cli` that build and
boot it — each built from its own checkout, and each guest digested
before the first boot so that two checkouts which turn out to be one
build are refused rather than timed twice. The run record names both
revisions of each (`*_git_sha` beside `*_inspector_git_sha`).
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path

from helios_bench import REPO_ROOT

# Where the baseline's warm build directory lives: under the candidate's
# `target/`, so the runner cache that restores `target/` restores the
# baseline kernel build with it. The checkout itself is not here (see
# `checkout_path`).
WORKTREES = REPO_ROOT / "target" / "perf-baselines" / "worktrees"
BUILD_DIR = "target"
# What `--baseline-ref` means when it is given without a value.
MERGE_BASE = "merge-base"
MERGE_BASE_AGAINST = "origin/dev"
ARTIFACTS = "artifacts"
# The vendored fork the kernel builds against, as a workspace path
# dependency on `../wasmtime/crates/wasmtime` (docs/wasmtime.md). The
# baseline checkout is the candidate's sibling so that its `../wasmtime`
# is this very directory — the same absolute path, hence the same cargo
# crate hashes and the same profile symbols (#359).
WASMTIME = "wasmtime"
# The baseline checkout's name beside the candidate's: `<candidate
# dir>-baseline-<sha12>`.
CHECKOUT_INFIX = "-baseline-"
# Where `helios-cli profile-fetch` keeps the kernel profile a release
# build reads, relative to a checkout (`helios-profdata`'s store). The
# baseline worktree links the candidate's rather than fetching its own,
# because a second fetch could resolve a newer collection than the
# candidate read and the pairing would then vary two things.
PROFILE_STORE = Path("target") / "profiles"


@dataclass(frozen=True)
class Baseline:
    """The commit the candidate is timed against, and where it was built."""

    ref: str
    sha: str
    worktree: Path


def git(*arguments: str, cwd: Path | None = None) -> str:
    # `REPO_ROOT` is read at call time, not bound as a default, so that a
    # test pointing the module at a repository of its own is obeyed.
    cwd = REPO_ROOT if cwd is None else cwd
    completed = subprocess.run(["git", *arguments], cwd=cwd, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        raise SystemExit(
            f"git {' '.join(arguments)} in {cwd} exited with status "
            f"{completed.returncode}: {completed.stderr.strip()}"
        )
    return completed.stdout.strip()


def resolve_sha(ref: str) -> str:
    """The commit a `--baseline-ref` names.

    Without a value the flag means the merge base with `dev`: the commit
    the branch is a change to, which is the only ref that makes the
    paired columns differ by the pull request and nothing else.
    """
    if ref == MERGE_BASE:
        return git("merge-base", "HEAD", MERGE_BASE_AGAINST)
    return git("rev-parse", "--verify", f"{ref}^{{commit}}")


def build_dir(sha: str) -> Path:
    """The baseline's warm `target/`, kept under the candidate's `target/`
    where the runner cache restores it."""
    return WORKTREES / sha / BUILD_DIR


def checkout_path(sha: str) -> Path:
    """The baseline checkout: the candidate's sibling, so that the
    kernel's `../wasmtime` path dependency names the candidate's vendored
    checkout at the same absolute path (#359)."""
    return REPO_ROOT.parent / f"{REPO_ROOT.name}{CHECKOUT_INFIX}{sha[:12]}"


def wasmtime_sibling() -> Path:
    """The vendored Wasmtime checkout both kernels compile against."""
    return REPO_ROOT.parent / WASMTIME


def resolve(ref: str) -> Baseline:
    """Where the baseline image will be built, without building it.

    Resolving is read-only so that `--dry-run` prints the plan a real run
    would execute, worktree path included, without creating one.
    """
    sha = resolve_sha(ref)
    if sha == git("rev-parse", "HEAD"):
        raise SystemExit(
            f"the baseline ref {ref} resolves to HEAD ({sha[:12]}); "
            "a run pairs a candidate with another commit, not with itself"
        )
    return Baseline(ref=ref, sha=sha, worktree=checkout_path(sha))


def prepare(baseline: Baseline) -> Path:
    """Creates or reuses the baseline worktree and everything it shares."""
    require_wasmtime_sibling()
    checkout = ensure_worktree(baseline.sha)
    link_build_dir(baseline.sha)
    link_profile_store(baseline.sha)
    link_missing(REPO_ROOT / ARTIFACTS, checkout / ARTIFACTS)
    return checkout


def ensure_worktree(sha: str) -> Path:
    """The worktree at `checkout_path(sha)`, the candidate's sibling.

    Reused when it is already there, registered to this repository, and
    still at that commit. The build it saves lives in `build_dir(sha)`
    and not in the checkout, so nothing under the checkout is ever
    deleted here: the path is outside the repository, beside the
    candidate, and a directory or file there that this repository does
    not list as its worktree is someone else's (§9) and stops the run
    naming it. A registered worktree at some other commit is refused
    the same way. The #328 shape — a checkout restored under `target/`
    by the runner cache into a repository that never registered it —
    cannot occur at this path, which the cache does not reach.
    """
    checkout = checkout_path(sha)
    if checkout.exists() or checkout.is_symlink():
        if not is_registered_worktree(checkout):
            raise SystemExit(
                f"{checkout} exists and is not a worktree of {REPO_ROOT}; "
                "the baseline checkout is created there, so move or remove it"
            )
        head = git("rev-parse", "HEAD", cwd=checkout)
        if head != sha:
            raise SystemExit(f"{checkout} is a worktree of {head}, not of {sha}")
        return checkout
    checkout.parent.mkdir(parents=True, exist_ok=True)
    git("worktree", "prune")
    git("worktree", "add", "--detach", str(checkout), sha)
    return checkout


def link_build_dir(sha: str) -> None:
    """Points the checkout's `target/` at the cached build directory.

    The checkout is the candidate's sibling and outside its `target/`;
    the build it accumulates is kept under the candidate's `target/`
    where the runner cache restores it, so a paired run on a warm cache
    pays for the baseline kernel once.
    """
    directory = build_dir(sha)
    directory.mkdir(parents=True, exist_ok=True)
    link_to(directory, checkout_path(sha) / BUILD_DIR)


def require_wasmtime_sibling() -> None:
    """The candidate's `../wasmtime` is the vendored checkout, or the run stops.

    The baseline checkout reaches the same directory through its own
    `../wasmtime` by being laid out beside the candidate; nothing is
    linked, so nothing can differ.
    """
    sibling = wasmtime_sibling()
    if not (sibling / "crates" / "wasmtime").is_dir():
        raise SystemExit(
            f"{sibling} is not the vendored Wasmtime checkout the workspace depends on; see docs/wasmtime.md"
        )


def is_registered_worktree(checkout: Path) -> bool:
    """Whether this repository lists `checkout` among its worktrees.

    Asked of the repository, not of the directory: a directory that is a
    repository of its own, or a worktree of another one, answers
    `git rev-parse --show-toplevel` with itself and would pass a check
    made from inside it. `git worktree list --porcelain` names exactly the
    paths this repository registered.
    """
    listing = git("worktree", "list", "--porcelain")
    registered = {
        Path(line.removeprefix("worktree ")).resolve()
        for line in listing.splitlines()
        if line.startswith("worktree ")
    }
    return checkout.resolve() in registered


def link_profile_store(sha: str) -> None:
    """Links the candidate's kernel profile store into the baseline worktree.

    The baseline's own inspector builds the baseline guest with
    `HELIOS_WORKSPACE_ROOT` naming the worktree, so an x86-64 release
    build reads the store under the worktree's own `target/`. Linking it
    to the candidate's means both images read the record in force when
    the run started, and a `baseline_ref` pairing attributes a difference
    to the commit and never to a difference of profile. The link is made
    whether or not the lane's target reads a profile: a target that does
    not never opens the store.
    """
    link_to(REPO_ROOT / PROFILE_STORE, checkout_path(sha) / PROFILE_STORE)


def link_to(source: Path, link: Path) -> None:
    if link.is_symlink():
        if link.readlink() == source:
            return
        link.unlink()
    elif link.exists():
        raise SystemExit(f"{link} exists and is not a link to {source}")
    link.parent.mkdir(parents=True, exist_ok=True)
    link.symlink_to(source)


def link_missing(source: Path, target: Path) -> None:
    """Links every entry of `source` the baseline checkout does not have.

    Entry by entry rather than the directory as a whole: the checkout
    tracks `artifacts/wasix/dash/dash.wasm` and its like from its own
    commit, and those are the baseline's own and stay. Everything else
    under `artifacts/` is staged by `tools/wasi-apps/build.sh` from
    pinned downloads, is identical for both images, and is linked rather
    than copied so that a paired run costs one CPython root and not two.
    """
    if not source.is_dir():
        raise SystemExit(f"{source} does not exist; run tools/wasi-apps/build.sh first")
    target.mkdir(parents=True, exist_ok=True)
    for entry in sorted(source.iterdir()):
        link = target / entry.name
        if link.is_symlink():
            if link.readlink() != entry:
                link.unlink()
                link.symlink_to(entry)
            continue
        if not link.exists():
            link.symlink_to(entry)
            continue
        if entry.is_dir() != link.is_dir():
            raise SystemExit(f"{link} and {entry} are not the same kind of entry")
        if entry.is_dir():
            link_missing(entry, link)

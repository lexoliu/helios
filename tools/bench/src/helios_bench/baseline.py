"""The baseline image of a paired run: a second Helios checkout whose
guest is built and timed beside the candidate's, in the same job.

A shared runner does not pin the CPU model, so two runs of one lane are
two machines as often as they are one machine twice: run 33990628290
reported every workload 20-40% faster than the `dev` run it was compared
against, including workloads its change could not touch (#173). The
answer is not a quieter runner but a second column taken on the same
machine.

The baseline checkout supplies a **guest**, never a harness. One harness
times both images — this checkout's `tools/wasi-apps/workload-bench.sh`,
its `helios-inspector` and its `helios-cli` — and an image is selected by
`HELIOS_WORKSPACE_ROOT`, the checkout the inspector resolves the guest
against. Two harnesses would compare the harnesses as much as the
kernels, and the older one need not even work: run 33995029872 failed
because the baseline checkout's inspector predated a fix to the host
side.

Shared, and therefore unable to explain a difference between the columns:

- the host, its CPU model, its load and its thermal state;
- the QEMU release, the accelerator, the vCPU count and the memory;
- the network backend and host HTTP, TCP and echo server implementations
  and payloads; each guest boot gets independent listeners, except for
  explicit diagnostic reuse of peer connection state;
- the harness: the benchmark script, the inspector binary that boots
  both guests, and the `helios-cli` that signs their prebuilt `cwasm`
  and builds their boot images;
- the workload manifest, read from the candidate checkout for both;
- everything under `artifacts/` that `tools/wasi-apps/build.sh` stages
  (the CPython root, the WASI tools, the WASIX programs), linked into the
  baseline worktree entry by entry rather than copied;
- the vendored Wasmtime checkout, linked as the worktree's sibling so
  that both kernels compile against one revision;
- the fetched kernel profile store (`target/profiles`, docs/pgo.md),
  linked into the worktree so that both x86-64 release kernels compile
  against the one profile in force when the run started (#321).

Per side, and therefore what the comparison measures: the kernel image,
the bootfs it carries (the compiler plugin included) and the guest
programs its prebuild signs — each built from its own checkout, and each
digested before the first boot so that two checkouts which turn out to
be one build are refused rather than timed twice.
"""

from __future__ import annotations

import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

from helios_bench import REPO_ROOT

WORKTREES = REPO_ROOT / "target" / "perf-baselines" / "worktrees"
# What `--baseline-ref` means when it is given without a value.
MERGE_BASE = "merge-base"
MERGE_BASE_AGAINST = "origin/dev"
ARTIFACTS = "artifacts"
# The vendored fork the kernel builds against, as a workspace path
# dependency on `../wasmtime/crates/wasmtime` (docs/wasmtime.md). The
# baseline worktree needs the same sibling, so it is laid out one
# directory below the worktree root.
WASMTIME = "wasmtime"
CHECKOUT = "helios"
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


def worktree_root(sha: str) -> Path:
    return WORKTREES / sha


def checkout_path(sha: str) -> Path:
    return worktree_root(sha) / CHECKOUT


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
    checkout = ensure_worktree(baseline.sha)
    link_wasmtime(baseline.sha)
    link_profile_store(baseline.sha)
    link_missing(REPO_ROOT / ARTIFACTS, checkout / ARTIFACTS)
    return checkout


def ensure_worktree(sha: str) -> Path:
    """The worktree at `target/perf-baselines/worktrees/<sha>/helios`.

    Reused when it is already there and still at that commit, because a
    kept worktree is a warm target directory and the build it saves is
    the largest fixed cost of a paired run.

    The directory can also come back without the repository knowing it:
    the runner cache restores `target/`, the worktree included, into a
    fresh checkout whose `.git/worktrees` never heard of it (#328). Git
    then resolves the directory to the enclosing repository, so its
    `HEAD` is the job's own. That shape is re-registered at `sha` with
    its `target/` kept, which is the cache the reuse exists for; only a
    registered worktree at some other commit is refused.
    """
    checkout = checkout_path(sha)
    if checkout.is_dir():
        if not is_registered_worktree(checkout):
            reregister_worktree(checkout, sha)
            return checkout
        head = git("rev-parse", "HEAD", cwd=checkout)
        if head != sha:
            raise SystemExit(f"{checkout} is a worktree of {head}, not of {sha}")
        return checkout
    checkout.parent.mkdir(parents=True, exist_ok=True)
    git("worktree", "prune")
    git("worktree", "add", "--detach", str(checkout), sha)
    return checkout


def is_registered_worktree(checkout: Path) -> bool:
    """Whether git resolves `checkout` to a worktree rooted there.

    A directory whose registration is gone resolves to the repository
    above it, or to nothing at all when its `.git` file still names the
    missing registration; neither is the worktree this run wants.
    """
    completed = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        cwd=checkout,
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        return False
    return Path(completed.stdout.strip()).resolve() == checkout.resolve()


def reregister_worktree(checkout: Path, sha: str) -> None:
    """Replaces an unregistered checkout with a worktree at `sha`, keeping its `target/`."""
    kept = checkout / "target"
    parked = checkout.parent / "target.kept"
    if parked.exists():
        shutil.rmtree(parked)
    if kept.is_dir():
        kept.rename(parked)
    shutil.rmtree(checkout)
    git("worktree", "prune")
    git("worktree", "add", "--detach", str(checkout), sha)
    if parked.is_dir():
        parked.rename(kept)


def link_wasmtime(sha: str) -> None:
    """Links the vendored Wasmtime checkout beside the baseline worktree.

    Both kernels compile against one revision by construction: a paired
    column that also changed Wasmtime would measure two things at once.
    """
    source = (REPO_ROOT.parent / WASMTIME).resolve()
    if not (source / "crates" / "wasmtime").is_dir():
        raise SystemExit(
            f"{source} is not the vendored Wasmtime checkout the workspace depends on; see docs/wasmtime.md"
        )
    link_to(source, worktree_root(sha) / WASMTIME)


def link_profile_store(sha: str) -> None:
    """Links the candidate's kernel profile store into the baseline worktree.

    The inspector that builds the baseline guest is the candidate's, with
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

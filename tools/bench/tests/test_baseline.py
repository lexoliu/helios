"""The baseline worktree shares what must not differ between the columns."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

from helios_bench import baseline


def test_the_baseline_worktree_reads_the_candidates_profile_store(tmp_path, monkeypatch):
    """Both x86-64 release kernels of a pairing read one profile record
    (#321): the worktree's store is a link to the candidate's, never a
    fetch of its own."""
    candidate = tmp_path / "candidate"
    (candidate / baseline.PROFILE_STORE).mkdir(parents=True)
    (candidate / baseline.PROFILE_STORE / "fetched.json").write_text("{}\n")
    monkeypatch.setattr(baseline, "REPO_ROOT", candidate)
    monkeypatch.setattr(baseline, "WORKTREES", candidate / "target" / "perf-baselines" / "worktrees")
    sha = "081e6f4c56aed629b0a09e6dc48e6a96e9c5719a"
    checkout = tmp_path / f"candidate-baseline-{sha[:12]}"
    checkout.mkdir(parents=True)
    baseline.link_build_dir(sha, checkout)

    baseline.link_profile_store(checkout)

    store = checkout / baseline.PROFILE_STORE
    assert store.is_symlink()
    assert store.readlink() == candidate / baseline.PROFILE_STORE
    assert (store / "fetched.json").read_text() == "{}\n"
    # The store link lives in the cached build directory, reached
    # through the checkout's `target/` link.
    assert (baseline.build_dir(sha) / "profiles").is_symlink()
    # Preparing the same worktree again keeps the link rather than refusing it.
    baseline.link_profile_store(checkout)
    assert store.readlink() == candidate / baseline.PROFILE_STORE


def test_a_real_store_in_the_worktree_is_refused_rather_than_replaced(tmp_path, monkeypatch):
    candidate = tmp_path / "candidate"
    (candidate / baseline.PROFILE_STORE).mkdir(parents=True)
    monkeypatch.setattr(baseline, "REPO_ROOT", candidate)
    monkeypatch.setattr(baseline, "WORKTREES", candidate / "target" / "perf-baselines" / "worktrees")
    sha = "081e6f4c56aed629b0a09e6dc48e6a96e9c5719a"
    checkout = tmp_path / f"candidate-baseline-{sha[:12]}"
    checkout.mkdir(parents=True)
    baseline.link_build_dir(sha, checkout)
    own_store = checkout / baseline.PROFILE_STORE
    own_store.mkdir(parents=True)

    try:
        baseline.link_profile_store(checkout)
    except SystemExit as refused:
        assert str(own_store) in str(refused)
    else:
        raise AssertionError("a store the worktree fetched itself is not silently replaced")
    assert Path(own_store).is_dir() and not own_store.is_symlink()


def git(*arguments: str, cwd: Path) -> str:
    return subprocess.run(
        ["git", *arguments], cwd=cwd, capture_output=True, text=True, check=True
    ).stdout.strip()


BASE_FORK = "2fb47b20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
CANDIDATE_FORK = "3f143bacbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"


def pin_wasmtime(root: Path, revision: str) -> None:
    """Writes the pin the way `.github/actions/checkout-wasmtime` states it."""
    action = root / baseline.WASMTIME_ACTION
    action.parent.mkdir(parents=True, exist_ok=True)
    action.write_text(
        "name: Checkout the pinned Wasmtime tree\n"
        "runs:\n"
        "  using: composite\n"
        "  steps:\n"
        "    - uses: actions/checkout@v4\n"
        "      with:\n"
        "        repository: lexoliu/wasmtime\n"
        f"        ref: {revision}\n"
        "        path: wasmtime\n"
    )


def repository_with_two_commits(root: Path, candidate_fork: str = BASE_FORK) -> tuple[str, str]:
    """A base and a candidate commit; the candidate moves the fork pin
    to `candidate_fork` when that differs from the base's."""
    root.mkdir()
    git("init", "-q", "-b", "dev", cwd=root)
    git("config", "user.email", "bench@helios.test", cwd=root)
    git("config", "user.name", "bench", cwd=root)
    git("config", "commit.gpgsign", "false", cwd=root)
    (root / "a.txt").write_text("base\n")
    pin_wasmtime(root, BASE_FORK)
    git("add", "-A", cwd=root)
    git("commit", "-q", "-m", "base", cwd=root)
    base = git("rev-parse", "HEAD", cwd=root)
    (root / "a.txt").write_text("candidate\n")
    pin_wasmtime(root, candidate_fork)
    git("commit", "-q", "-am", "candidate", cwd=root)
    return base, git("rev-parse", "HEAD", cwd=root)


def fork_checkout(path: Path, revision: str) -> None:
    """A stand-in fork checkout whose HEAD is `revision`'s commit object:
    a repository with one commit, which the test then names by its sha."""
    (path / "crates" / "wasmtime").mkdir(parents=True)
    git("init", "-q", str(path), cwd=path.parent)
    git("config", "user.email", "bench@helios.test", cwd=path)
    git("config", "user.name", "bench", cwd=path)
    git("config", "commit.gpgsign", "false", cwd=path)
    (path / "crates" / "wasmtime" / "lib.rs").write_text(f"// {revision}\n")
    git("add", "-A", cwd=path)
    git("commit", "-q", "-m", revision, cwd=path)


def test_the_baseline_checkout_is_the_candidates_sibling(tmp_path, monkeypatch):
    """The kernel depends on `../wasmtime/crates/wasmtime` by path, and
    cargo hashes a path dependency outside the workspace by its absolute
    path (#359). A baseline that reached the vendored checkout through a
    link under `target/` compiled every Wasmtime crate under another crate
    hash and matched none of the profile's symbols; laid out beside the
    candidate, its `../wasmtime` is the candidate's, absolute path and
    all."""
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")
    (tmp_path / "wasmtime" / "crates" / "wasmtime").mkdir(parents=True)
    (repo / baseline.ARTIFACTS).mkdir()
    (repo / baseline.ARTIFACTS / "python3-root").mkdir()

    resolved = baseline.resolve(base)
    assert resolved.fork_root == ""
    checkout = baseline.prepare(resolved)

    assert checkout == resolved.worktree
    assert checkout.parent == repo.parent
    assert checkout.name == f"helios-baseline-{base[:12]}"
    # Lexically the same directory, not merely the same after resolving
    # links: the old layout's linked sibling also resolved to it.
    assert checkout.parent / "wasmtime" == repo.parent / "wasmtime"
    assert not checkout.is_symlink() and not (checkout.parent / "wasmtime").is_symlink()
    assert not any(part.is_symlink() for part in [checkout, *checkout.parents])
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    assert (checkout / "target").is_symlink()
    assert (checkout / "target").readlink() == baseline.build_dir(base)
    assert (checkout / baseline.ARTIFACTS / "python3-root").is_symlink()


def test_a_missing_wasmtime_sibling_stops_the_run_before_the_worktree(tmp_path, monkeypatch):
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")

    resolved = baseline.resolve(base)
    try:
        baseline.prepare(resolved)
    except SystemExit as refused:
        assert str(tmp_path / "wasmtime") in str(refused)
    else:
        raise AssertionError("a baseline without the vendored checkout beside it is not built")
    assert not resolved.worktree.exists()


def test_a_baseline_pinning_another_fork_builds_against_its_own(tmp_path, monkeypatch):
    """Run 38968194856 built the baseline of a pin bump against the
    candidate's fork and the kernel did not link (#486). A baseline whose
    commit pins another revision is laid out beside that revision, in
    `wasmtime-fork-<revision12>/`, at the absolute path its profile
    collection used (#359) — never beside the candidate's."""
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo, candidate_fork=CANDIDATE_FORK)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")
    (tmp_path / "wasmtime" / "crates" / "wasmtime").mkdir(parents=True)
    (repo / baseline.ARTIFACTS).mkdir()

    resolved = baseline.resolve(base)

    assert resolved.wasmtime == baseline.WasmtimePin(repository="lexoliu/wasmtime", revision=BASE_FORK)
    assert resolved.fork_root == f"wasmtime-fork-{BASE_FORK[:12]}"
    fork = tmp_path / resolved.fork_root / "wasmtime"
    assert resolved.wasmtime_checkout == fork
    assert resolved.worktree == tmp_path / resolved.fork_root / f"helios-baseline-{base[:12]}"

    # Not checked out yet: the refusal names the fork, the revision and where.
    with pytest.raises(SystemExit, match=BASE_FORK) as refused:
        baseline.prepare(resolved)
    assert str(fork) in str(refused.value) and "lexoliu/wasmtime" in str(refused.value)
    assert not resolved.worktree.exists()

    # Checked out at some other commit: refused, never built against.
    fork.parent.mkdir()
    fork_checkout(fork, BASE_FORK)
    head = git("rev-parse", "HEAD", cwd=fork)
    with pytest.raises(SystemExit, match=head):
        baseline.prepare(resolved)

    # At the pinned commit: the baseline checkout's `../wasmtime` is it.
    pinned = baseline.Baseline(
        ref="dev",
        sha=base,
        worktree=resolved.worktree,
        wasmtime=baseline.WasmtimePin(repository="lexoliu/wasmtime", revision=head),
        fork_root=resolved.fork_root,
    )
    checkout = baseline.prepare(pinned)
    assert checkout.parent / "wasmtime" == fork
    assert not any(part.is_symlink() for part in [checkout, *checkout.parents])
    assert git("rev-parse", "HEAD", cwd=checkout) == base


def test_the_fork_a_commit_pins_is_read_from_that_commit(tmp_path, monkeypatch):
    repo = tmp_path / "helios"
    base, candidate = repository_with_two_commits(repo, candidate_fork=CANDIDATE_FORK)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)

    assert baseline.pinned_wasmtime(base).revision == BASE_FORK
    assert baseline.pinned_wasmtime(candidate).revision == CANDIDATE_FORK
    # The candidate's own pin lays out beside it; another pin gets its root.
    assert baseline.fork_root(baseline.pinned_wasmtime(candidate)) == ""
    assert baseline.fork_root(baseline.pinned_wasmtime(base)) == f"wasmtime-fork-{BASE_FORK[:12]}"


def test_a_cached_build_directory_is_reused_by_a_fresh_checkout(tmp_path, monkeypatch):
    """The runner cache brings `target/` back — the baseline's warm build
    directory under it included — into a job whose repository never
    registered the previous run's checkout (#328). The checkout is
    recreated at the baseline commit and its `target/` link lands on the
    build the cache carried, which is what made caching it worthwhile."""
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")

    checkout = baseline.resolve(base).worktree
    baseline.ensure_worktree(base, checkout)
    baseline.link_build_dir(base, checkout)
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    warm = checkout / "target" / "x86_64-unknown-none" / "release" / "helios"
    warm.parent.mkdir(parents=True)
    warm.write_bytes(b"warm kernel")

    # A fresh runner: the checkout is gone and so is its registration;
    # the cache restored `target/` with the build directory in it.
    shutil.rmtree(checkout)
    for registration in (repo / ".git" / "worktrees").iterdir():
        for entry in sorted(registration.rglob("*"), reverse=True):
            entry.unlink() if entry.is_file() else entry.rmdir()
        registration.rmdir()
    cached = baseline.build_dir(base) / "x86_64-unknown-none" / "release" / "helios"
    assert cached.read_bytes() == b"warm kernel"

    again = baseline.ensure_worktree(base, checkout)
    baseline.link_build_dir(base, checkout)

    assert again == checkout
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    assert git("rev-parse", "--show-toplevel", cwd=checkout) == str(checkout.resolve())
    assert warm.read_bytes() == b"warm kernel", "the build the cache carried is still there"
    assert (checkout / "a.txt").read_text() == "base\n"


def test_a_directory_this_repository_did_not_register_is_never_touched(tmp_path, monkeypatch):
    """The checkout path is beside the candidate, outside the repository.
    Whatever is there that this repository does not list as its worktree
    — a repository of someone else's, which would answer
    `--show-toplevel` with itself, or a plain file — stops the run naming
    the path and is left exactly as found (§9)."""
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")
    other = baseline.resolve(base).worktree
    git("init", "-q", str(other), cwd=tmp_path)
    (other / "theirs.txt").write_text("not ours\n")

    with pytest.raises(SystemExit, match=str(other)):
        baseline.ensure_worktree(base, other)
    assert (other / "theirs.txt").read_text() == "not ours\n"

    shutil.rmtree(other)
    other.write_text("a file\n")
    with pytest.raises(SystemExit, match=str(other)):
        baseline.ensure_worktree(base, other)
    assert other.read_text() == "a file\n"


def test_a_registered_worktree_at_another_commit_is_still_refused(tmp_path, monkeypatch):
    repo = tmp_path / "helios"
    base, candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")
    checkout = baseline.ensure_worktree(base, baseline.resolve(base).worktree)
    git("checkout", "-q", "--detach", candidate, cwd=checkout)

    try:
        baseline.ensure_worktree(base, checkout)
    except SystemExit as refused:
        assert candidate in str(refused) and base in str(refused)
    else:
        raise AssertionError("a worktree the repository knows at another commit is not silently reused")

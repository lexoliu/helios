"""The baseline worktree shares what must not differ between the columns."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

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
    baseline.checkout_path(sha).mkdir(parents=True)
    baseline.link_build_dir(sha)

    baseline.link_profile_store(sha)

    store = baseline.checkout_path(sha) / baseline.PROFILE_STORE
    assert store.is_symlink()
    assert store.readlink() == candidate / baseline.PROFILE_STORE
    assert (store / "fetched.json").read_text() == "{}\n"
    # The store link lives in the cached build directory, reached
    # through the checkout's `target/` link.
    assert (baseline.build_dir(sha) / "profiles").is_symlink()
    # Preparing the same worktree again keeps the link rather than refusing it.
    baseline.link_profile_store(sha)
    assert store.readlink() == candidate / baseline.PROFILE_STORE


def test_a_real_store_in_the_worktree_is_refused_rather_than_replaced(tmp_path, monkeypatch):
    candidate = tmp_path / "candidate"
    (candidate / baseline.PROFILE_STORE).mkdir(parents=True)
    monkeypatch.setattr(baseline, "REPO_ROOT", candidate)
    monkeypatch.setattr(baseline, "WORKTREES", candidate / "target" / "perf-baselines" / "worktrees")
    sha = "081e6f4c56aed629b0a09e6dc48e6a96e9c5719a"
    baseline.checkout_path(sha).mkdir(parents=True)
    baseline.link_build_dir(sha)
    own_store = baseline.checkout_path(sha) / baseline.PROFILE_STORE
    own_store.mkdir(parents=True)

    try:
        baseline.link_profile_store(sha)
    except SystemExit as refused:
        assert str(own_store) in str(refused)
    else:
        raise AssertionError("a store the worktree fetched itself is not silently replaced")
    assert Path(own_store).is_dir() and not own_store.is_symlink()


def git(*arguments: str, cwd: Path) -> str:
    return subprocess.run(
        ["git", *arguments], cwd=cwd, capture_output=True, text=True, check=True
    ).stdout.strip()


def repository_with_two_commits(root: Path) -> tuple[str, str]:
    root.mkdir()
    git("init", "-q", "-b", "dev", cwd=root)
    git("config", "user.email", "bench@helios.test", cwd=root)
    git("config", "user.name", "bench", cwd=root)
    git("config", "commit.gpgsign", "false", cwd=root)
    (root / "a.txt").write_text("base\n")
    git("add", "a.txt", cwd=root)
    git("commit", "-q", "-m", "base", cwd=root)
    base = git("rev-parse", "HEAD", cwd=root)
    (root / "a.txt").write_text("candidate\n")
    git("commit", "-q", "-am", "candidate", cwd=root)
    return base, git("rev-parse", "HEAD", cwd=root)


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

    checkout = baseline.prepare(baseline.Baseline(ref="dev", sha=base, worktree=baseline.checkout_path(base)))

    assert checkout.parent == repo.parent
    assert checkout.name == f"helios-baseline-{base[:12]}"
    assert (checkout / ".." / "wasmtime").resolve() == (repo / ".." / "wasmtime").resolve()
    assert not (checkout / ".." / "wasmtime").is_symlink() and not (checkout.parent / "wasmtime").is_symlink()
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    assert (checkout / "target").is_symlink()
    assert (checkout / "target").readlink() == baseline.build_dir(base)
    assert (checkout / baseline.ARTIFACTS / "python3-root").is_symlink()


def test_a_missing_wasmtime_sibling_stops_the_run_before_the_worktree(tmp_path, monkeypatch):
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")

    try:
        baseline.prepare(baseline.Baseline(ref="dev", sha=base, worktree=baseline.checkout_path(base)))
    except SystemExit as refused:
        assert str(tmp_path / "wasmtime") in str(refused)
    else:
        raise AssertionError("a baseline without the vendored checkout beside it is not built")
    assert not baseline.checkout_path(base).exists()


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

    checkout = baseline.ensure_worktree(base)
    baseline.link_build_dir(base)
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

    again = baseline.ensure_worktree(base)
    baseline.link_build_dir(base)

    assert again == checkout
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    assert git("rev-parse", "--show-toplevel", cwd=checkout) == str(checkout.resolve())
    assert warm.read_bytes() == b"warm kernel", "the build the cache carried is still there"
    assert (checkout / "a.txt").read_text() == "base\n"


def test_a_checkout_git_no_longer_resolves_is_replaced(tmp_path, monkeypatch):
    """A directory at the checkout path that is not a registered worktree
    — a repository re-cloned since, the shape #328 met — holds nothing the
    run needs: its build lives in the cached directory. It is replaced by
    a worktree at the baseline commit."""
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")
    stale = baseline.checkout_path(base)
    stale.mkdir(parents=True)
    (stale / "a.txt").write_text("stale\n")

    checkout = baseline.ensure_worktree(base)

    assert checkout == stale
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    assert (checkout / "a.txt").read_text() == "base\n"


def test_a_registered_worktree_at_another_commit_is_still_refused(tmp_path, monkeypatch):
    repo = tmp_path / "helios"
    base, candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")
    checkout = baseline.ensure_worktree(base)
    git("checkout", "-q", "--detach", candidate, cwd=checkout)

    try:
        baseline.ensure_worktree(base)
    except SystemExit as refused:
        assert candidate in str(refused) and base in str(refused)
    else:
        raise AssertionError("a worktree the repository knows at another commit is not silently reused")

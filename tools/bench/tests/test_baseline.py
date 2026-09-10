"""The baseline worktree shares what must not differ between the columns."""

from __future__ import annotations

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
    monkeypatch.setattr(baseline, "WORKTREES", tmp_path / "worktrees")
    sha = "081e6f4c56aed629b0a09e6dc48e6a96e9c5719a"
    (baseline.checkout_path(sha) / "target").mkdir(parents=True)

    baseline.link_profile_store(sha)

    store = baseline.checkout_path(sha) / baseline.PROFILE_STORE
    assert store.is_symlink()
    assert store.readlink() == candidate / baseline.PROFILE_STORE
    assert (store / "fetched.json").read_text() == "{}\n"
    # Preparing the same worktree again keeps the link rather than refusing it.
    baseline.link_profile_store(sha)
    assert store.readlink() == candidate / baseline.PROFILE_STORE


def test_a_real_store_in_the_worktree_is_refused_rather_than_replaced(tmp_path, monkeypatch):
    candidate = tmp_path / "candidate"
    (candidate / baseline.PROFILE_STORE).mkdir(parents=True)
    monkeypatch.setattr(baseline, "REPO_ROOT", candidate)
    monkeypatch.setattr(baseline, "WORKTREES", tmp_path / "worktrees")
    sha = "081e6f4c56aed629b0a09e6dc48e6a96e9c5719a"
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


def test_a_cached_worktree_that_lost_its_registration_is_reused_with_its_target(tmp_path, monkeypatch):
    """The runner cache brings `target/` back, the baseline worktree
    inside it included, into a fresh checkout that never registered it
    (#328). Git then answers the enclosing repository's HEAD for it, which
    is the candidate's merge commit and not the baseline's; the run
    re-registers the directory at the baseline commit and keeps the warm
    `target/` that made caching it worthwhile."""
    repo = tmp_path / "helios"
    base, _candidate = repository_with_two_commits(repo)
    monkeypatch.setattr(baseline, "REPO_ROOT", repo)
    monkeypatch.setattr(baseline, "WORKTREES", repo / "target" / "perf-baselines" / "worktrees")

    checkout = baseline.ensure_worktree(base)
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    warm = checkout / "target" / "x86_64-unknown-none" / "release" / "helios"
    warm.parent.mkdir(parents=True)
    warm.write_bytes(b"warm kernel")

    # The cache restored the files; the fresh checkout has no registration.
    (checkout / ".git").unlink()
    for registration in (repo / ".git" / "worktrees").iterdir():
        for entry in sorted(registration.rglob("*"), reverse=True):
            entry.unlink() if entry.is_file() else entry.rmdir()
        registration.rmdir()
    assert git("rev-parse", "HEAD", cwd=checkout) != base, "the shape the runner produced"

    again = baseline.ensure_worktree(base)

    assert again == checkout
    assert git("rev-parse", "HEAD", cwd=checkout) == base
    assert git("rev-parse", "--show-toplevel", cwd=checkout) == str(checkout.resolve())
    assert warm.read_bytes() == b"warm kernel", "the build the cache carried is still there"
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

"""The baseline worktree shares what must not differ between the columns."""

from __future__ import annotations

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

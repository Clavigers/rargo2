import subprocess

from factory.drift import drift, line


def git(repo, *args):
    subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *args],
                   check=True, capture_output=True)


def test_status_says_when_the_running_code_is_not_mains(tmp_path):
    repo, here = tmp_path / "repo", tmp_path / "src"
    (repo / "dev/factory/factory").mkdir(parents=True)
    (repo / "dev/factory/factory/a.py").write_text("a = 1\n")
    (repo / "dev/factory/factory/b.py").write_text("b = 1\n")
    (repo / "other.rs").write_text("fn main() {}\n")
    git(repo, "init", "-q", "-b", "main")
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "one")
    git(repo, "update-ref", "refs/remotes/origin/main", "HEAD")
    (here / "factory/__pycache__").mkdir(parents=True)
    (here / "factory/a.py").write_text("a = 1\n")
    (here / "factory/b.py").write_text("b = 1\n")
    (here / "factory/__pycache__/a.cpython-312.pyc").write_bytes(b"\0")
    assert drift(here, repo) == [] and line(here, repo) is None

    (here / "factory/b.py").write_text("b = 2\n")  # changed by hand
    (here / "factory/c.py").write_text("")  # not on main
    (repo / "dev/factory/factory/a.py").unlink()  # main moved on: a.py removed, d.py added
    (repo / "dev/factory/factory/d.py").write_text("d = 1\n")
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "two")
    git(repo, "update-ref", "refs/remotes/origin/main", "HEAD")
    assert drift(here, repo) == ["factory/a.py", "factory/b.py", "factory/c.py", "factory/d.py"]
    said = line(here, repo)
    assert said.startswith(f"deploy: {here} differs from dev/factory on origin/main (")
    assert said.endswith("4 files: factory/a.py, factory/b.py, factory/c.py, factory/d.py")


def test_status_says_when_it_cannot_compare(tmp_path):
    assert line(tmp_path, tmp_path / "no-repo").startswith("deploy: cannot compare")

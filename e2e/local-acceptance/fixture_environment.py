"""Build a private home and an isolated environment for acceptance fixtures."""
import atexit
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def create_fixture_environment(source, *, seed_prepared_tools):
    """Clear hook-exported repository selectors before touching temporary repos.

    Tiny executable profiles use the fixture's private home and lease files.
    Never fabricate or copy an enclosing host's acquisition claim.
    """
    fixture_home = tempfile.TemporaryDirectory(prefix="scode-enforcement-home-")
    atexit.register(fixture_home.cleanup)
    names = subprocess.check_output(["git", "rev-parse", "--local-env-vars"],
                                    cwd=source, text=True).splitlines()
    environment = {key: value for key, value in os.environ.items() if key not in names
                   and not key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))}
    environment["PYTHONIOENCODING"] = "utf-8"
    environment.setdefault("CARGO_HOME", str(Path.home() / ".cargo"))
    environment.setdefault("RUSTUP_HOME", str(Path.home() / ".rustup"))
    if seed_prepared_tools:
        # Keep prepared public tool archives when isolating HOME. Every consumer still
        # verifies its pinned archive and executable. Copy bytes so corruption
        # controls cannot modify the enclosing cache, Git state or lease files.
        for tool in ("nextest", "sccache"):
            cache = Path(".cache/sudocode-local-acceptance/tools") / tool
            prepared = Path.home() / cache
            if prepared.is_dir():
                shutil.copytree(prepared, Path(fixture_home.name) / cache)
    environment.pop("SUDOCODE_ACCEPTANCE_HOST_LEASE", None)
    # Callers also merge this mapping into os.environ; an empty value clears
    # their inherited claim in both that path and an explicit subprocess env.
    environment.update(SUDOCODE_ACCEPTANCE_HOST_LEASE="", NO_COLOR="1",
                       HOME=fixture_home.name, USERPROFILE=fixture_home.name)
    return fixture_home, environment

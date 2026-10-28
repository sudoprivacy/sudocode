#!/usr/bin/env python3
"""Exercise the actual CLI build.rs in dependency-free, nested Cargo fixtures."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
CLI_BINARY = "scode"


class Fixture:
    def __init__(self, root: Path, source: Path, env: dict[str, str]):
        self.root, self.env = root, env.copy()
        self.package = root / "nested" / "cli"
        self.step = 0
        if not self.package.exists():
            (self.package / "src").mkdir(parents=True)
            (root / "Cargo.toml").write_text('[workspace]\nmembers=["nested/cli"]\nresolver="2"\n', encoding="utf-8")
            (root / ".gitignore").write_text("target/\n*.stdout.log\n*.stderr.log\n", encoding="utf-8")
            (self.package / "Cargo.toml").write_text(
                '[package]\nname="metadata-fixture"\nversion="0.0.0"\nedition="2021"\n'
                # Production build.rs can emit linker directives for this target.
                f'[[bin]]\nname="{CLI_BINARY}"\npath="src/main.rs"\n', encoding="utf-8"
            )
            (self.package / "src/main.rs").write_text(
                'fn main() { println!("{}|{}", env!("GIT_SHA"), env!("BUILD_DATE")); }\n', encoding="utf-8"
            )
            # Include the unmodified production source; only the invocation
            # counter is fixture code. OUT_DIR is the build script's output.
            include = str(source)
            (self.package / "build.rs").write_text(
                f'mod production {{ include!(r#"{include}"#); pub fn run() {{ main(); }} }}\n'
                'fn main() {\n'
                ' let path=std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("count");\n'
                ' let count=std::fs::read_to_string(&path).ok().and_then(|s|s.parse::<u32>().ok()).unwrap_or(0)+1;\n'
                ' std::fs::write(path,count.to_string()).unwrap();\n'
                ' production::run();\n}\n', encoding="utf-8"
            )

    def command(self, *args: str) -> str:
        self.step += 1
        result = subprocess.run(args, cwd=self.root, env=self.env, text=True,
                                encoding="utf-8", errors="replace", capture_output=True)
        prefix = self.root.parent / f"{self.root.name}-{self.step:02}"
        prefix.with_suffix(".stdout.log").write_text(result.stdout, encoding="utf-8")
        prefix.with_suffix(".stderr.log").write_text(result.stderr, encoding="utf-8")
        if result.returncode:
            print(result.stderr[-16_384:], file=sys.stderr, end="")
            raise AssertionError(f"{args[0]} exited {result.returncode}; logs: {prefix}")
        return result.stdout.strip()

    def git(self, *args: str) -> str:
        return self.command("git", "-c", "user.name=Build fixture", "-c",
                            "user.email=build-fixture@example.invalid", "-c",
                            "commit.gpgsign=false", "-c", "core.hooksPath=", *args)

    def commit(self) -> str:
        self.git("commit", "--allow-empty", "-m", "Change only Git identity")
        return self.git("rev-parse", "--short", "HEAD")

    def build(self, expected_count: int, expected_sha: str, verify_paths: bool = True) -> None:
        self.command("cargo", "build", "--offline", "--jobs", "1", "-vv",
                     "--manifest-path", str(self.package / "Cargo.toml"),
                     "--target-dir", str(self.root / "target"))
        counters = list((self.root / "target/debug/build").glob("metadata-fixture-*/out/count"))
        assert len(counters) == 1, counters
        count = int(counters[0].read_text(encoding="utf-8"))
        assert count == expected_count, f"{self.root.name}: build script ran {count} times, expected {expected_count}"
        output = counters[0].parent.parent / "output"
        for line in output.read_text(encoding="utf-8").splitlines() if verify_paths else []:
            if line.startswith("cargo:rerun-if-changed="):
                path = Path(line.split("=", 1)[1])
                path = path if path.is_absolute() else self.package / path
                assert path.exists(), f"Build script watches missing path: {path}"
        binary = self.root / "target/debug" / f"{CLI_BINARY}{'.exe' if os.name == 'nt' else ''}"
        assert self.command(str(binary)) == f"{expected_sha}|{self.env['BUILD_DATE']}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--build-script", type=Path, default=ROOT / "rust/crates/rusty-sudocode-cli/build.rs")
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    source = args.build_script.resolve(strict=True)
    evidence = args.output_dir or Path(os.environ.get("BUILD_METADATA_EVIDENCE", ""))
    if str(evidence) == ".":
        evidence = Path(tempfile.mkdtemp(prefix="scode-build-metadata-"))
    else:
        evidence = evidence.resolve()
        evidence.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "SOURCE_DATE_EPOCH", "CARGO_TARGET_DIR"):
        env.pop(key, None)
    env.update(BUILD_DATE="2026-10-19", CARGO_INCREMENTAL="0", CARGO_BUILD_JOBS="1",
               CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0", CARGO_TERM_COLOR="never")
    report = dict(source=str(source), source_sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
                  checks=[], success=False)
    try:
        normal = Fixture(evidence / "normal repository", source, env)
        normal.git("init")
        normal.git("add", ".")
        sha = normal.commit()
        normal.build(1, sha, verify_paths=False)
        normal.build(1, sha)
        report["checks"].append("nested normal repository reuses unchanged build")
        sha = normal.commit()
        normal.build(2, sha)
        report["checks"].append("loose branch commit refreshes embedded SHA")
        normal.git("pack-refs", "--all", "--prune")
        normal.build(3, sha)
        normal.build(3, sha)
        sha = normal.commit()
        normal.build(4, sha)
        report["checks"].append("packed refs stay fresh and detect a new loose branch ref")

        linked_root = evidence / "linked worktree"
        normal.git("worktree", "add", "-b", "fixture-linked", str(linked_root))
        linked = Fixture(linked_root, source, env)
        linked.build(1, sha)
        linked.build(1, sha)
        sha = linked.commit()
        linked.build(2, sha)
        report["checks"].append("linked worktree finds both private HEAD and common branch ref")
        linked.git("checkout", "--detach")
        linked.build(3, sha)
        sha = linked.commit()
        linked.build(4, sha)
        linked.build(4, sha)
        report["checks"].append("detached HEAD refreshes SHA and reuses unchanged build")

        archive = Fixture(evidence / "source archive", source, env)
        archive.build(1, "unknown")
        archive.build(1, "unknown")
        archive.env["BUILD_DATE"] = "2026-10-20"
        archive.build(2, "unknown")
        archive.env["SOURCE_DATE_EPOCH"] = "1792454400"
        archive.build(3, "unknown")
        report["checks"].append("source archive reuses build and tracks both date environment inputs")
        report["success"] = True
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        (evidence / "result.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        print(json.dumps(dict(success=report["success"], checks=len(report["checks"]), evidence=str(evidence))))


if __name__ == "__main__":
    main()

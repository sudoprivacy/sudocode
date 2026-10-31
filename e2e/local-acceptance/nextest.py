#!/usr/bin/env python3
"""Run the policy-owned native executor integration guard."""
import runpy
from pathlib import Path


if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).resolve().parents[2] /
                       ".github/local-acceptance/nextest_guard.py"), run_name="__main__")

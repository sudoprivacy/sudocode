"""Compare the production response codec with flate2 using real native files."""
import argparse
import gzip
import importlib.util
import json
import os
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location("codec_executor", ROOT / ".github/scripts/local_acceptance.py")
    executor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(executor)
    executor.ROOT = ROOT
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "rust/target")).resolve()
    command = ["cargo", "build", "--locked", "--manifest-path", str(Path(__file__).with_name("Cargo.toml")),
               "--target-dir", str(target)]
    executor.run_command(command, ROOT / "rust", output / "build.log")
    binary = target / "debug" / ("response-gzip-acceptance.exe" if os.name == "nt" else "response-gzip-acceptance")
    for mode in ["old", "new"]:
        executor.run_command([str(binary), mode, str(output)], ROOT, output / f"{mode}.log")
    for index in range(42):
        expected = bytes((n * 17 + index * 71) % 256 for n in range(
            0 if index % 6 == 0 else 1024 * 1024 if index % 6 == 1 else 600 + index))
        old = (output / "old" / f"{index}.gz").read_bytes()
        new = (output / "new" / f"{index}.gz").read_bytes()
        assert old == new, f"compressed bytes changed at capture {index}"
        assert gzip.decompress(new) == expected, f"independent decoder rejected capture {index}"
    for limit in [0, 5, 10, 15, 20, 30, 40, 50, 70, 1000]:
        for suffix in ["gz", "txt"]:
            assert (output / "old" / f"quota-{limit}.{suffix}").read_bytes() == (
                output / "new" / f"quota-{limit}.{suffix}").read_bytes(), f"quota behavior changed: {limit}/{suffix}"
    result = {"status": "PASS", "files": 42, "concurrent_captures": 6,
              "all_sinks_released_before_reuse": True, "compressed_bytes_identical": True,
              "python_and_native_decoders_passed": True, "quota_boundaries_and_prefixes_identical": True,
              "scope": "Actual production codec/native files. CLI and provider behavior require their own real PTY acceptance."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()

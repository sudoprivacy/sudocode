"""Strip a release binary in place and keep its symbols in a separate archive.

The release-artifact gate runs after this, so it accepts the exact bytes that
ship. The archive holds what a debugger needs to name the stripped frames:

- Linux (ELF): `objcopy --only-keep-debug` writes `<binary>.debug`, holding the
  symbol table, and the binary is stripped with a `.gnu_debuglink` to it.
- macOS (Mach-O): the unstripped binary itself, for `atos`/`lldb`. Stripping
  invalidates the linker's ad-hoc signature, which arm64 requires, so the
  stripped binary is signed ad hoc again.
- Windows (PE): the executable has no symbol table; names are in the PDB the
  linker wrote beside it. The executable is left as built.

Usage: split_release_symbols.py <binary> <archive.tar.gz>
"""

import argparse
import pathlib
import shutil
import subprocess
import sys
import tarfile


def run(*command):
    subprocess.run(command, check=True)


def output(*command):
    return subprocess.run(command, check=True, capture_output=True, text=True).stdout


def tool(name):
    path = shutil.which(name)
    if not path:
        sys.exit(f"split_release_symbols: {name} is required on this runner")
    return path


def elf(binary):
    objcopy, readelf = tool("objcopy"), tool("readelf")
    debug = binary.with_name(binary.name + ".debug")
    run(objcopy, "--only-keep-debug", str(binary), str(debug))
    run(objcopy, "--strip-all", f"--add-gnu-debuglink={debug}", str(binary))
    if ".symtab" not in output(readelf, "-S", "-W", str(debug)):
        sys.exit("split_release_symbols: the symbol file has no symbol table")
    if ".symtab" in output(readelf, "-S", "-W", str(binary)):
        sys.exit("split_release_symbols: the binary still has a symbol table")
    return [debug]


def mach_o(binary):
    strip, codesign, nm = tool("strip"), tool("codesign"), tool("nm")
    symbols = binary.with_name(binary.name + ".unstripped")
    shutil.copy2(binary, symbols)
    run(strip, str(binary))
    run(codesign, "--force", "--sign", "-", str(binary))
    run(codesign, "--verify", str(binary))
    if not output(nm, str(symbols)).strip():
        sys.exit("split_release_symbols: the symbol file has no symbols")
    return [symbols]


def pe(binary):
    pdb = binary.with_suffix(".pdb")
    if not pdb.is_file():
        sys.exit(f"split_release_symbols: no PDB beside {binary}")
    return [pdb]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=pathlib.Path)
    parser.add_argument("archive", type=pathlib.Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    with binary.open("rb") as head:
        magic = head.read(4)
    before = binary.stat().st_size
    if magic == b"\x7fELF":
        files = elf(binary)
    elif magic in (b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe"):
        files = mach_o(binary)
    elif magic[:2] == b"MZ":
        files = pe(binary)
    else:
        sys.exit(f"split_release_symbols: unrecognised executable format {magic!r}")
    args.archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(args.archive, "w:gz") as archive:
        for path in files:
            archive.add(path, arcname=path.name)
    after = binary.stat().st_size
    print(f"{binary.name}: {before:,} -> {after:,} bytes; symbols in {args.archive.name}")


if __name__ == "__main__":
    main()

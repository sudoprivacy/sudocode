"""Replay captured fixture output through WriteConsoleW without application changes."""
import ctypes
from ctypes import wintypes
import json
from pathlib import Path
import sys
import time

kernel = ctypes.WinDLL("kernel32", use_last_error=True)
kernel.GetStdHandle.restype = wintypes.HANDLE
kernel.GetConsoleMode.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
kernel.SetConsoleMode.argtypes = [wintypes.HANDLE, wintypes.DWORD]
kernel.WriteConsoleW.argtypes = [wintypes.HANDLE, wintypes.LPCWSTR, wintypes.DWORD,
                               ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
output = kernel.GetStdHandle(-11)
mode = wintypes.DWORD()
assert kernel.GetConsoleMode(output, ctypes.byref(mode))
assert kernel.SetConsoleMode(output, mode.value | 4)
input_handle = kernel.GetStdHandle(-10)
if kernel.GetConsoleMode(input_handle, ctypes.byref(mode)):
    assert kernel.SetConsoleMode(input_handle, (mode.value & ~7) | 0x200)

frames = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))[sys.argv[2]]
for frame in frames:
    chunks = frame.splitlines(keepends=True) if sys.argv[3] == "lines" else [frame]
    for chunk in chunks:
        count = wintypes.DWORD()
        assert kernel.WriteConsoleW(output, chunk, len(chunk.encode("utf-16-le")) // 2,
                                   ctypes.byref(count), None)
    time.sleep(0.04)
Path(sys.argv[4]).write_text(json.dumps({"variant": sys.argv[2], "writes": sys.argv[3],
                                       "frames": len(frames)}), encoding="utf-8")
release = Path(sys.argv[5])
deadline = time.monotonic() + 10
while not release.exists() and time.monotonic() < deadline:
    time.sleep(0.02)

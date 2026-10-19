"""Observe console cursor columns around the same full-width row used by prose."""
import ctypes
from ctypes import wintypes
import json
from pathlib import Path
import sys
import time


class Coord(ctypes.Structure):
    _fields_ = [("x", wintypes.SHORT), ("y", wintypes.SHORT)]


class Rect(ctypes.Structure):
    _fields_ = [("left", wintypes.SHORT), ("top", wintypes.SHORT),
                ("right", wintypes.SHORT), ("bottom", wintypes.SHORT)]


class Info(ctypes.Structure):
    _fields_ = [("size", Coord), ("cursor", Coord), ("attributes", wintypes.WORD),
                ("window", Rect), ("maximum", Coord)]


kernel = ctypes.WinDLL("kernel32", use_last_error=True)
kernel.GetStdHandle.restype = wintypes.HANDLE
kernel.GetConsoleMode.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
kernel.SetConsoleMode.argtypes = [wintypes.HANDLE, wintypes.DWORD]
kernel.WriteConsoleW.argtypes = [wintypes.HANDLE, wintypes.LPCWSTR, wintypes.DWORD,
                               ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
kernel.GetConsoleScreenBufferInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Info)]
handle = kernel.GetStdHandle(-11)
mode = wintypes.DWORD()
assert kernel.GetConsoleMode(handle, ctypes.byref(mode))
original = mode.value
new_mode = (original | 4) & ~8
if sys.argv[2] == "pending":
    new_mode |= 8
assert kernel.SetConsoleMode(handle, new_mode)


def write(text):
    count = wintypes.DWORD()
    assert kernel.WriteConsoleW(handle, text, len(text.encode("utf-16-le")) // 2,
                               ctypes.byref(count), None)


def cursor():
    info = Info()
    assert kernel.GetConsoleScreenBufferInfo(handle, ctypes.byref(info))
    return [info.cursor.x, info.cursor.y]


line = "• ProseStart 中文 é 💻 ordinary text with bold words, inline code, and a link (https://example.com). More ordinary prose"
write("\x1b[0m" + line[:-1])
before = cursor()
if sys.argv[1] == "positioned":
    write("\x1b[120G")
if sys.argv[1] != "plain":
    write("\x1b[K")
write(line[-1] + "\x1b[0m")
after = cursor()
write("\r\nROW_PROBE_DONE")
final = cursor()
Path(sys.argv[3]).write_text(json.dumps({"variant": sys.argv[1], "mode": sys.argv[2],
    "original_mode": original, "effective_mode": new_mode,
    "before": before, "after": after, "final": final}), encoding="utf-8")
release = Path(sys.argv[4])
deadline = time.monotonic() + 10
while not release.exists() and time.monotonic() < deadline:
    time.sleep(0.02)

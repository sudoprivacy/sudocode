"""Own one isolated native TUI's Windows ConPTY; stdout is a small JSON control channel.

The same contract as nativeCodexTui.py, on a real pseudoconsole instead of a
Unix PTY: argv is the TUI command, stdin takes {"action": "close"} lines, stdout
reports spawn/trace/exit JSON lines, and NATIVE_LIFECYCLE_PTY_TRACE receives the
current output tail. Only the Windows API is used (ctypes), so the kit needs no
extra package.
"""
import ctypes
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time
from ctypes import wintypes

kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)

EXTENDED_STARTUPINFO_PRESENT = 0x00080000
STARTF_USESTDHANDLES = 0x00000100
PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = 0x00020016
WAIT_OBJECT_0 = 0
CURSOR_FORWARD = re.compile(rb'\x1b\[(\d*)C')


class COORD(ctypes.Structure):
    _fields_ = [('X', wintypes.SHORT), ('Y', wintypes.SHORT)]


class STARTUPINFOW(ctypes.Structure):
    _fields_ = [
        ('cb', wintypes.DWORD), ('lpReserved', wintypes.LPWSTR), ('lpDesktop', wintypes.LPWSTR),
        ('lpTitle', wintypes.LPWSTR), ('dwX', wintypes.DWORD), ('dwY', wintypes.DWORD),
        ('dwXSize', wintypes.DWORD), ('dwYSize', wintypes.DWORD), ('dwXCountChars', wintypes.DWORD),
        ('dwYCountChars', wintypes.DWORD), ('dwFillAttribute', wintypes.DWORD), ('dwFlags', wintypes.DWORD),
        ('wShowWindow', wintypes.WORD), ('cbReserved2', wintypes.WORD), ('lpReserved2', ctypes.c_void_p),
        ('hStdInput', wintypes.HANDLE), ('hStdOutput', wintypes.HANDLE), ('hStdError', wintypes.HANDLE),
    ]


class STARTUPINFOEXW(ctypes.Structure):
    _fields_ = [('StartupInfo', STARTUPINFOW), ('lpAttributeList', ctypes.c_void_p)]


class PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [('hProcess', wintypes.HANDLE), ('hThread', wintypes.HANDLE),
                ('dwProcessId', wintypes.DWORD), ('dwThreadId', wintypes.DWORD)]


kernel32.CreatePipe.argtypes = [ctypes.POINTER(wintypes.HANDLE), ctypes.POINTER(wintypes.HANDLE), ctypes.c_void_p, wintypes.DWORD]
kernel32.CreatePseudoConsole.argtypes = [COORD, wintypes.HANDLE, wintypes.HANDLE, wintypes.DWORD, ctypes.POINTER(ctypes.c_void_p)]
kernel32.CreatePseudoConsole.restype = ctypes.c_long
kernel32.ClosePseudoConsole.argtypes = [ctypes.c_void_p]
kernel32.InitializeProcThreadAttributeList.argtypes = [ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(ctypes.c_size_t)]
kernel32.UpdateProcThreadAttribute.argtypes = [ctypes.c_void_p, wintypes.DWORD, ctypes.c_size_t, ctypes.c_void_p,
                                               ctypes.c_size_t, ctypes.c_void_p, ctypes.c_void_p]
kernel32.DeleteProcThreadAttributeList.argtypes = [ctypes.c_void_p]
kernel32.CreateProcessW.argtypes = [wintypes.LPCWSTR, wintypes.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, wintypes.BOOL,
                                    wintypes.DWORD, ctypes.c_void_p, wintypes.LPCWSTR, ctypes.c_void_p,
                                    ctypes.POINTER(PROCESS_INFORMATION)]
kernel32.ReadFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
kernel32.WriteFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
kernel32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
kernel32.WaitForSingleObject.restype = wintypes.DWORD
kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
kernel32.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]


def check(ok, what):
    if not ok:
        raise ctypes.WinError(ctypes.get_last_error(), what)


def pipe():
    read, write = wintypes.HANDLE(), wintypes.HANDLE()
    check(kernel32.CreatePipe(ctypes.byref(read), ctypes.byref(write), None, 0), 'CreatePipe')
    return read, write


# The pseudoconsole reads keys from one pipe and renders the TUI into the other.
console_in, keys = pipe()
screen, console_out = pipe()
pseudoconsole = ctypes.c_void_p()
result = kernel32.CreatePseudoConsole(COORD(120, 36), console_in, console_out, 0, ctypes.byref(pseudoconsole))
if result != 0:
    raise OSError(f'CreatePseudoConsole failed: HRESULT {result & 0xFFFFFFFF:#010x}')
# The pseudoconsole owns its ends now; keeping ours would hide end-of-output.
kernel32.CloseHandle(console_in)
kernel32.CloseHandle(console_out)

size = ctypes.c_size_t()
kernel32.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(size))
attributes = ctypes.create_string_buffer(size.value)
check(kernel32.InitializeProcThreadAttributeList(attributes, 1, 0, ctypes.byref(size)), 'InitializeProcThreadAttributeList')
check(kernel32.UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, pseudoconsole,
                                         ctypes.sizeof(ctypes.c_void_p), None, None), 'UpdateProcThreadAttribute')
startup = STARTUPINFOEXW()
startup.StartupInfo.cb = ctypes.sizeof(STARTUPINFOEXW)
# Null standard handles: the TUI must use the pseudoconsole, never this driver's
# redirected control pipes.
startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES
startup.lpAttributeList = ctypes.cast(attributes, ctypes.c_void_p)
process = PROCESS_INFORMATION()
command_line = ctypes.create_unicode_buffer(subprocess.list2cmdline(sys.argv[1:]))
check(kernel32.CreateProcessW(None, command_line, None, None, False, EXTENDED_STARTUPINFO_PRESENT,
                              None, None, ctypes.byref(startup), ctypes.byref(process)), 'CreateProcessW')
kernel32.CloseHandle(process.hThread)
pid = process.dwProcessId

output = queue.Queue()


def read_screen():
    buffer = ctypes.create_string_buffer(65536)
    count = wintypes.DWORD()
    while kernel32.ReadFile(screen, buffer, len(buffer), ctypes.byref(count), None) and count.value:
        output.put(buffer.raw[:count.value])
    output.put(None)


close_requested = threading.Event()


def read_control():
    for line in sys.stdin:
        if json.loads(line).get('action') == 'close':
            break
    close_requested.set()


threading.Thread(target=read_screen, daemon=True).start()
threading.Thread(target=read_control, daemon=True).start()


def send(data):
    written = wintypes.DWORD()
    kernel32.WriteFile(keys, data, len(data), ctypes.byref(written), None)


def running():
    return kernel32.WaitForSingleObject(process.hProcess, 0) != WAIT_OBJECT_0


def wait(seconds):
    return kernel32.WaitForSingleObject(process.hProcess, int(seconds * 1000)) == WAIT_OBJECT_0


tail = b''
closing = None
quit_sent = False
quit_keys = b'/exit\r'
next_key_at = None
trace_path = os.environ.get('NATIVE_LIFECYCLE_PTY_TRACE')


def publish_trace(record):
    if trace_path:
        temporary = trace_path + '.tmp'
        with open(temporary, 'w') as trace_file:
            json.dump({**record, 'outputTail': tail.decode('utf-8', errors='replace')}, trace_file)
        # Windows refuses to replace a file another process has open, and the
        # orchestrator reads this one while polling; it holds it for one read.
        for _ in range(100):
            try:
                os.replace(temporary, trace_path)
                return
            except PermissionError:
                time.sleep(0.01)
        os.replace(temporary, trace_path)


def trace(event):
    record = {'event': event, 'at': time.time(), 'pid': pid}
    publish_trace(record)
    print(json.dumps(record), flush=True)


def take(data):
    global tail
    # The pseudoconsole re-renders the TUI and writes runs of blank cells as
    # cursor-forward (CSI n C) instead of the spaces the TUI wrote. Restore them,
    # so the tail carries the same text a Unix PTY would.
    data = CURSOR_FORWARD.sub(lambda match: b' ' * int(match.group(1) or 1), data)
    tail = (tail + data)[-16000:]
    publish_trace({'pid': pid})
    if b'\x1b[6n' in data:
        send(b'\x1b[1;1R')
    if b'\x1b[?u' in data:
        send(b'\x1b[?0u')


print(json.dumps({'pid': pid, 'event': 'spawn'}), flush=True)
returncode = None
try:
    while running():
        if closing is not None and time.monotonic() - closing > 8:
            trace('normal_exit_timeout')
            break
        try:
            data = output.get(timeout=0.1)
        except queue.Empty:
            data = b''
        if data:
            take(data)
        if close_requested.is_set() and closing is None:
            closing = time.monotonic()
            trace('close_received')
        if closing is not None and not quit_sent and '›'.encode() in tail:
            # Match typed keys, not one burst that the native paste detector
            # correctly treats as a multiline draft rather than a command.
            if next_key_at is None or time.monotonic() >= next_key_at:
                send(quit_keys[:1])
                quit_keys = quit_keys[1:]
                next_key_at = time.monotonic() + 0.04
                if not quit_keys:
                    quit_sent = True
                    trace('quit_sent')
    if not wait(3):
        # On Windows terminate and kill are the same TerminateProcess; both are
        # reported so the orchestrator fails the case either way.
        print(json.dumps({'event': 'terminate', 'at': time.time()}), flush=True)
        kernel32.TerminateProcess(process.hProcess, 1)
        if not wait(3):
            print(json.dumps({'event': 'kill', 'at': time.time()}), flush=True)
            kernel32.TerminateProcess(process.hProcess, 1)
            wait(3)
finally:
    if running():
        kernel32.TerminateProcess(process.hProcess, 1)
        wait(3)
    code = wintypes.DWORD()
    if kernel32.GetExitCodeProcess(process.hProcess, ctypes.byref(code)):
        returncode = code.value
    # Closing the pseudoconsole ends its output; keep draining so it cannot stall.
    closer = threading.Thread(target=kernel32.ClosePseudoConsole, args=(pseudoconsole,), daemon=True)
    closer.start()
    end = time.monotonic() + 3
    while time.monotonic() < end:
        try:
            data = output.get(timeout=0.1)
        except queue.Empty:
            if not closer.is_alive():
                break
            continue
        if data is None:
            break
        take(data)
    closer.join(1)
    kernel32.DeleteProcThreadAttributeList(attributes)
    kernel32.CloseHandle(keys)
    kernel32.CloseHandle(process.hProcess)
    print(json.dumps({'pid': pid, 'event': 'exit', 'returncode': returncode,
                      'outputTail': tail.decode('utf-8', errors='replace')}), flush=True)

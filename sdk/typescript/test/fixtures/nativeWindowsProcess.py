"""Bounded native Windows lookups for the isolated Codex mailbox fixture.

Uses the installed SDK's MIB_TCPROW_OWNER_PID/GetExtendedTcpTable and
QueryFullProcessImageNameW contracts. Never opens a process for mutation.
"""
import ctypes
from ctypes import wintypes
import json
import socket
import struct
import sys


class TcpRow(ctypes.Structure):
    _fields_ = [(name, wintypes.DWORD) for name in
                ('state', 'local_address', 'local_port', 'remote_address', 'remote_port', 'pid')]


def owner_pid(local_port, remote_port):
    if not 0 < local_port < 65536 or not 0 < remote_port < 65536:
        raise ValueError('Invalid TCP port')
    api = ctypes.WinDLL('iphlpapi', use_last_error=True).GetExtendedTcpTable
    api.argtypes = [ctypes.c_void_p, ctypes.POINTER(wintypes.DWORD), wintypes.BOOL,
                    wintypes.ULONG, ctypes.c_int, wintypes.ULONG]
    api.restype = wintypes.DWORD
    size = wintypes.DWORD(0)
    result = api(None, ctypes.byref(size), False, socket.AF_INET, 5, 0)
    if result != 122:
        raise OSError(result, 'GetExtendedTcpTable size query')
    # The table can grow between calls; a bounded resize is not a message retry.
    for _ in range(3):
        if not 4 <= size.value <= 16*1024*1024:
            raise ValueError('Unexpected TCP table size')
        buffer = ctypes.create_string_buffer(size.value)
        result = api(buffer, ctypes.byref(size), False, socket.AF_INET, 5, 0)
        if result == 122:
            continue
        if result:
            raise OSError(result, 'GetExtendedTcpTable')
        count = wintypes.DWORD.from_buffer_copy(buffer).value
        if 4 + count*ctypes.sizeof(TcpRow) > len(buffer):
            raise ValueError('Truncated TCP table')
        pids = set()
        for index in range(count):
            row = TcpRow.from_buffer_copy(buffer, 4+index*ctypes.sizeof(TcpRow))
            local = socket.inet_ntoa(struct.pack('<I', row.local_address))
            remote = socket.inet_ntoa(struct.pack('<I', row.remote_address))
            if (row.state == 5 and local == remote == '127.0.0.1'
                    and socket.ntohs(row.local_port & 0xffff) == local_port
                    and socket.ntohs(row.remote_port & 0xffff) == remote_port):
                pids.add(row.pid)
        if len(pids) != 1 or 0 in pids:
            raise ValueError('Expected exactly one owner of the established loopback tuple')
        return pids.pop()
    raise RuntimeError('TCP table changed beyond the bounded lookup')


def executable(pid):
    if pid <= 0:
        raise ValueError('Invalid process ID')
    api = ctypes.WinDLL('kernel32', use_last_error=True)
    api.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    api.OpenProcess.restype = wintypes.HANDLE
    api.CloseHandle.argtypes = [wintypes.HANDLE]
    api.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                             wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)]
    api.QueryFullProcessImageNameW.restype = wintypes.BOOL
    handle = api.OpenProcess(0x1000, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        size = wintypes.DWORD(32768)
        buffer = ctypes.create_unicode_buffer(size.value)
        if not api.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(size)):
            raise ctypes.WinError(ctypes.get_last_error())
        return buffer.value
    finally:
        api.CloseHandle(handle)


if __name__ == '__main__':
    if len(sys.argv) == 4 and sys.argv[1] == 'tcp':
        result = owner_pid(int(sys.argv[2]), int(sys.argv[3]))
    elif len(sys.argv) == 3 and sys.argv[1] == 'exe':
        result = executable(int(sys.argv[2]))
    else:
        raise ValueError('Expected tcp localPort remotePort or exe pid')
    print(json.dumps(result))

"""Own a real native AF_UNIX listener while a Node child inspects its address."""
from contextlib import ExitStack
import ctypes
import os
import socket
import subprocess
import sys


endpoint = os.path.join(sys.argv[4], 'control.sock')
with ExitStack() as cleanup:
    bound = False
    cleanup.callback(lambda: os.unlink(endpoint) if bound else None)
    if os.name == 'nt':
        # CPython's Windows socket module need not expose AF_UNIX. Use the
        # actual Winsock provider, rather than a named pipe or a skipped test.
        # WSADATA: learn.microsoft.com/windows/win32/api/winsock2/ns-winsock2-wsadata
        class WSAData(ctypes.Structure):
            limits = [('iMaxSockets', ctypes.c_ushort), ('iMaxUdpDg', ctypes.c_ushort),
                      ('lpVendorInfo', ctypes.c_void_p)]
            strings = [('szDescription', ctypes.c_char * 257), ('szSystemStatus', ctypes.c_char * 129)]
            _fields_ = [('wVersion', ctypes.c_ushort), ('wHighVersion', ctypes.c_ushort)] + (
                limits + strings if ctypes.sizeof(ctypes.c_void_p) == 8 else strings + limits)

        class UnixAddress(ctypes.Structure):
            _fields_ = [('sun_family', ctypes.c_ushort), ('sun_path', ctypes.c_char * 108)]

        winsock = ctypes.WinDLL('Ws2_32.dll')

        def api(name, arguments, result=ctypes.c_int):
            function = getattr(winsock, name)
            function.argtypes, function.restype = arguments, result
            return function

        error = api('WSAGetLastError', [])

        def checked(result, operation):
            if result != 0:
                raise OSError(error(), operation)

        data = WSAData()
        started = api('WSAStartup', [ctypes.c_ushort, ctypes.POINTER(WSAData)])(0x0202, ctypes.byref(data))
        if started:
            raise OSError(started, 'WSAStartup')
        cleanup.callback(lambda: checked(api('WSACleanup', [])(), 'WSACleanup'))
        assert data.wVersion == 0x0202
        handle = api('socket', [ctypes.c_int, ctypes.c_int, ctypes.c_int], ctypes.c_size_t)(1, 1, 0)
        if handle == ctypes.c_size_t(-1).value:
            raise OSError(error(), 'socket(AF_UNIX)')
        cleanup.callback(lambda: checked(api('closesocket', [ctypes.c_size_t])(handle), 'closesocket'))
        encoded = endpoint.encode('utf-8')
        if len(encoded) >= 108 or b'\0' in encoded:
            raise ValueError('Fixture AF_UNIX path exceeds the native sockaddr_un capacity')
        address = UnixAddress(1, encoded)
        checked(api('bind', [ctypes.c_size_t, ctypes.c_void_p, ctypes.c_int])(
            handle, ctypes.byref(address), ctypes.sizeof(address)), 'bind(AF_UNIX)')
        bound = True
        checked(api('listen', [ctypes.c_size_t, ctypes.c_int])(handle, 1), 'listen(AF_UNIX)')
    else:
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        cleanup.callback(listener.close)
        listener.bind(endpoint)
        bound = True
        listener.listen(1)
    result = subprocess.run([sys.argv[1], '-e', sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]], timeout=8)
raise SystemExit(result.returncode)

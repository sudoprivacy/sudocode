"""Own one isolated native TUI's PTY; stdout is a small JSON control channel."""
import fcntl
import json
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time
from nativeTuiPrompt import update_prompt_visible

master, slave = pty.openpty()
os.set_blocking(master, False)
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 120, 0, 0))
def session():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
child = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave, preexec_fn=session)
os.close(slave)
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
        os.replace(temporary, trace_path)
def trace(event):
    record = {'event': event, 'at': time.time(), 'pid': child.pid}
    publish_trace(record)
    print(json.dumps(record), flush=True)
print(json.dumps({'pid': child.pid, 'event': 'spawn'}), flush=True)
try:
    while child.poll() is None:
        if closing is not None and time.monotonic() - closing > 8:
            trace('normal_exit_timeout')
            break
        watched = [master] if closing is not None else [master, sys.stdin]
        ready, _, _ = select.select(watched, [], [], 0.1)
        if master in ready:
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            tail = (tail + data)[-16000:]
            publish_trace({'pid': child.pid})
            if b'\x1b[6n' in data:
                try:
                    os.write(master, b'\x1b[1;1R')
                except BlockingIOError:
                    pass
            if b'\x1b[?u' in data:
                try:
                    os.write(master, b'\x1b[?0u')
                except BlockingIOError:
                    pass
        if update_prompt_visible(tail):
            trace('unsafe_update_prompt')
            break
        if sys.stdin in ready:
            line = sys.stdin.readline()
            if not line or json.loads(line).get('action') == 'close':
                closing = time.monotonic()
                trace('close_received')
        if closing is not None and not quit_sent and '›'.encode() in tail:
            # Match typed keys, not one burst that the native paste detector
            # correctly treats as a multiline draft rather than a command.
            if next_key_at is None or time.monotonic() >= next_key_at:
                os.write(master, quit_keys[:1])
                quit_keys = quit_keys[1:]
                next_key_at = time.monotonic() + 0.04
                if not quit_keys:
                    quit_sent = True
                    trace('quit_sent')
    try:
        child.wait(timeout=3)
    except subprocess.TimeoutExpired:
        print(json.dumps({'event': 'terminate', 'at': time.time()}), flush=True)
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            print(json.dumps({'event': 'kill', 'at': time.time()}), flush=True)
            child.kill()
            child.wait(timeout=3)
finally:
    if child.poll() is None:
        child.kill()
        child.wait(timeout=3)
    os.close(master)
    print(json.dumps({'pid': child.pid, 'event': 'exit', 'returncode': child.returncode,
                      'outputTail': tail.decode('utf-8', errors='replace')}), flush=True)

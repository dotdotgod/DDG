#!/usr/bin/env python3
"""Run Xcode's stdio MCP proxy with one private LLDB backend."""
import os
import selectors
import signal
import subprocess
import sys
import tempfile
import threading
import time


def wait_for_backend(backend):
    deadline = time.monotonic() + 10
    output = b''
    with selectors.DefaultSelector() as ready:
        ready.register(backend.stdout, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            if not ready.select(max(0, deadline - time.monotonic())):
                break
            chunk = os.read(backend.stdout.fileno(), 4096)
            if not chunk:
                break
            output += chunk
            if b'MCP server started with connection listeners:' in output:
                return
    print(output.decode(errors='replace'), end='', file=sys.stderr)
    raise RuntimeError('Xcode LLDB MCP backend did not start')


def main():
    def terminate(signum, _):
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminate)
    with tempfile.TemporaryDirectory(prefix='ddg-lldb-mcp-', dir='/tmp') as home:
        # Xcode's proxy needs exactly one discoverable backend; don't use the user's registry.
        env = dict(os.environ, HOME=home)
        backend = subprocess.Popen(['/usr/bin/xcrun', 'lldb', '--no-lldbinit', '--no-use-colors',
                                    '-o', f'protocol-server start MCP accept://{home}/socket'],
                                   env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
        def log_backend():
            for data in iter(lambda: backend.stdout.read(4096), b''):
                print(data.decode(errors='replace'), end='', file=sys.stderr)

        logs = threading.Thread(target=log_backend, daemon=True)
        try:
            wait_for_backend(backend)
            logs.start()
            return subprocess.run(['/usr/bin/xcrun', 'lldb-mcp'], env=env).returncode
        finally:
            # Quit gracefully so LLDB destroys any outstanding debug sessions.
            try:
                backend.stdin.write(b'quit\n')
            except BrokenPipeError:
                pass
            backend.stdin.close()
            try:
                backend.wait(timeout=3)
            except subprocess.TimeoutExpired:
                backend.kill()
                backend.wait()
            if logs.ident is not None:
                logs.join(timeout=1)
            backend.stdout.close()


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, RuntimeError) as exc:
        print(exc, file=sys.stderr)
        sys.exit(1)

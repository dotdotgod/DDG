import argparse
import json
import os
import re
import signal
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ANSI = re.compile(r'\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-_])')


class DebugError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def emit(data):
    print(json.dumps(data, allow_nan=False))


def error(exc, as_json):
    if isinstance(exc, subprocess.TimeoutExpired):
        exc = DebugError(124, f'{exc.cmd[0]} timed out after {exc.timeout}s')
    elif isinstance(exc, FileNotFoundError):
        exc = DebugError(4, f'missing tool: {exc.filename}')
    elif isinstance(exc, OSError):
        exc = DebugError(3, str(exc))
    message = ANSI.sub('', str(exc))
    print(message, file=sys.stderr)
    if as_json:
        emit({'error': message, 'code': exc.code})
    return exc.code


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise DebugError(2, message)


def run(command, timeout=300, *, env=None, ok=(0,), logs=True, cwd=ROOT, with_code=False):
    with subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                          encoding='utf-8', errors='replace', start_new_session=True) as proc:
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.communicate()
            raise
    stdout, stderr = (ANSI.sub('', output) for output in (stdout, stderr))
    if logs and stderr:
        print(stderr, end='', file=sys.stderr)
    if ok is not None and proc.returncode not in ok:
        if logs and stdout:
            print(stdout, end='', file=sys.stderr)
        raise DebugError(3, f'{command[0]} exited {proc.returncode}: {stderr.strip() or stdout.strip()}')
    return (stdout, proc.returncode) if with_code else stdout


def bounded(low, high):
    def parse(value):
        result = int(value, 10)
        if not low <= result <= high:
            raise argparse.ArgumentTypeError(f'expected {low}–{high}')
        return result
    return parse

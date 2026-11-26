"""Compose lifecycle and non-interactive Linux debugger commands."""
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from xml.etree import ElementTree as ET

from debug_common import ROOT, DebugError, run
from xctrace_summarize import valgrind_summarize

ARCHES = ('arm64', 'amd64')
COMPOSE = ['docker', 'compose', '--file', str(ROOT / 'docker/docker-compose.debug.yml')]


def service(arch):
    return f'ddg-debug-{arch}'


def container(arch, running=True):
    command = [*COMPOSE, 'ps', '--all', '--quiet', service(arch)]
    ids = run(command, timeout=15).split()
    if not ids:
        if not running:
            return None, 'absent'
        raise DebugError(3, f'container is absent; run linux-up --arch {arch}')
    status = run(['docker', 'inspect', '--format', '{{.State.Status}}', ids[0]], timeout=15).strip()
    if running and status != 'running':
        raise DebugError(3, f'{service(arch)} is {status}; run linux-up --arch {arch}')
    return ids[0], status


def start(arch, build=False, env=None):
    output = run([*COMPOSE, 'up', '--detach', *(['--build'] if build else []), service(arch)],
                 timeout=1800, env=env)
    if output:
        print(output, end='', file=sys.stderr)
    identifier, status = container(arch)
    return {'arch': arch, 'service': service(arch), 'container': identifier, 'status': status}


def linux_up(args):
    return start(args.arch, args.build), 0


def linux_down(args):
    arches = ARCHES if args.arch == 'all' else (args.arch,)
    results = []
    if args.arch == 'all':
        command = [*COMPOSE, '--profile', 'arm64', '--profile', 'amd64', 'down']
    else:
        command = [*COMPOSE, 'rm', '--stop', '--force', service(args.arch)]
    for arch in arches:
        identifier, _ = container(arch, running=False)
        results.append({'arch': arch, 'service': service(arch), 'container': identifier,
                        'status': 'removed' if identifier else 'absent'})
    output = run(command, timeout=60)
    if output:
        print(output, end='', file=sys.stderr)
    return {'arch': args.arch, 'containers': results}, 0


def endpoint(arch):
    address = run([*COMPOSE, 'port', service(arch), '1234'], timeout=15).strip()
    if not address.startswith('127.0.0.1:') or not address.partition(':')[2].isdigit():
        raise DebugError(3, f'expected a single loopback port for {service(arch)}, got {address!r}')
    return address


def linux_dap(args):
    identifier, status = container(args.arch, running=False)
    if status != 'running' or (args.port and endpoint(args.arch) != f'127.0.0.1:{args.port}'):
        env = dict(os.environ)
        if args.port:
            env[f'DDG_DEBUG_PORT_{args.arch.upper()}'] = str(args.port)
        identifier = start(args.arch, env=env)['container']
    address = endpoint(args.arch)
    for _ in range(10):
        _, code = run(['docker', 'exec', identifier, 'python3', '-c',
                       'import socket; socket.create_connection(("127.0.0.1", 1234), 1).close()'],
                      timeout=5, ok=None, with_code=True, logs=False)
        if code == 0:
            return {'arch': args.arch, 'endpoint': address, 'container': identifier,
                    'dap_hint': f'xcrun lldb-dap; use platform select remote-linux and '
                                f'platform connect connect://{address} for the platform handshake. '
                                'Use in-container inspect for inferior launches without extra port forwarding.'}, 0
        time.sleep(0.2)
    raise DebugError(3, f'lldb-server did not listen in {service(args.arch)}')


def inspect(args):
    identifier, _ = container(args.os.removeprefix('linux-'))
    path = args.bin
    if path.startswith('target/debug/'):
        path = '/opt/ddg-target/debug/' + path.removeprefix('target/debug/')
    command = ['docker', 'exec', identifier, '/usr/bin/python3', '/workspace/scripts/debug/lldb_inspect.py',
               '--os', args.os, '--bin', path, '--break', args.breakpoint,
               '--watch-size', str(args.watch_size), '--asm-count', str(args.asm_count),
               '--step-inst', str(args.step_inst), '--timeout', str(args.timeout)]
    if args.watch is not None:
        command += ['--watch', args.watch]
    for expr, size in args.mem:
        command += ['--mem', f'{expr}:{size}']
    if args.args:
        command += ['--args', '--', *args.args]
    stdout, code = run(command, timeout=args.timeout + 12, ok=None, with_code=True)
    try:
        result = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise DebugError(3, 'container LLDB did not return a JSON snapshot') from exc
    if code:
        raise DebugError(code if code in (2, 4, 124) else 3, result.get('error', 'container LLDB failed'))
    return result


def memory(args):
    if args.tool not in ('valgrind', 'heaptrack'):
        raise DebugError(2, 'Linux mem requires --tool valgrind or heaptrack')
    if args.tool == 'valgrind' and args.pid:
        raise DebugError(2, 'Valgrind cannot attach to a PID; use --cmd or heaptrack --pid')
    if not args.pid and not args.cmd:
        raise DebugError(2, '--cmd requires an executable')
    identifier, _ = container(args.os.removeprefix('linux-'))
    docker = ['docker', 'exec', identifier]
    directory = run(docker + ['mktemp', '-d', '/tmp/ddg-mem.XXXXXXXX'], timeout=10).strip()
    report = {'os': args.os, 'tool': args.tool, 'pid': args.pid}
    try:
        if args.tool == 'valgrind':
            path = f'{directory}/valgrind.xml'
            output, code = run(docker + ['sh', '-c', 'exec 3>"$1"; shift; exec "$@"', 'sh', path,
                                         'timeout', '-k', '5s', '120s', 'valgrind', '--leak-check=full',
                                         '--show-leak-kinds=all', '--xml=yes', '--xml-fd=3', '--', *args.cmd],
                               timeout=130, ok=None, with_code=True)
            if output:
                print(output, end='', file=sys.stderr)
            if code in (124, 137):
                raise DebugError(124, 'Valgrind timed out')
            xml = run(docker + ['cat', path], timeout=15)
            try:
                report.update(valgrind_summarize(xml))
            except (ET.ParseError, ValueError) as exc:
                raise DebugError(3, f'invalid Valgrind report (tool/target exit {code}): {exc}') from exc
            raw = Path(tempfile.mkdtemp(prefix='ddg-valgrind-')) / 'valgrind.xml'
            raw.write_text(xml, encoding='utf-8')
            report.update(raw_xml_path=str(raw), target_exit=code)
            return report, 3 if code or report['error_count'] or not report['finished'] else 0

        # heaptrack writes a compressed file with a version-dependent suffix and PID.
        target = ['-p', str(args.pid)] if args.pid else ['--', *args.cmd]
        seconds = 10 if args.pid else 120
        output, code = run(docker + ['timeout', '-s', 'INT', '-k', '5s', f'{seconds}s',
                                     'heaptrack', '-o', f'{directory}/profile', *target],
                           timeout=seconds + 12, ok=None, with_code=True)
        if output:
            print(output, end='', file=sys.stderr)
        if code == 137 or (code == 124 and not args.pid):
            raise DebugError(124, 'heaptrack timed out')
        files = run(docker + ['find', directory, '-type', 'f'], timeout=10).splitlines()
        profiles = [name for name in files if name.endswith(('.gz', '.zst'))]
        if len(profiles) != 1 or code not in (0, 124):
            raise DebugError(3, f'heaptrack did not produce one profile (exit {code})')
        text = run(docker + ['heaptrack_print', '-f', profiles[0]], timeout=30).strip()
        keys = ('calls to allocation functions', 'peak heap memory consumption',
                'peak RSS', 'total memory leaked', 'total runtime')
        report.update(summary={key: line.partition(':')[2].strip() for line in text.splitlines()
                               if (key := line.partition(':')[0].strip()) in keys}, output=text)
        return report, 0
    finally:
        try:
            run(docker + ['rm', '-rf', '--', directory], timeout=10)
        except (DebugError, subprocess.TimeoutExpired, OSError) as exc:
            print(f'cannot remove container scratch directory: {exc}', file=sys.stderr)

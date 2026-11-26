"""Headless GPUI tests and native CLI profiling."""
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path
from xml.etree import ElementTree as ET

from debug_common import ROOT, DebugError, run
from xctrace_summarize import TABLE_SCHEMAS, samply_summarize, summarize_exports, table_kind

TEMPLATES = {'metal': 'Metal System Trace', 'hitches': 'Animation Hitches',
             'alloc': 'Allocations', 'time': 'Time Profiler'}
METAL_ENV = {'MTL_DEBUG_LAYER': '1', 'MTL_SHADER_VALIDATION': '1'}


def gpui_test(args):
    if not shutil.which('cargo-nextest'):
        raise DebugError(4, 'missing cargo-nextest; cargo install --locked cargo-nextest')
    seed = args.seed if args.seed is not None else 0
    env = dict(os.environ, SEED=str(seed), NEXTEST_EXPERIMENTAL_LIBTEST_JSON='1')
    command = ['cargo', 'nextest', 'run', '--locked', '-p', 'ddg-agents-native',
               '-E', 'test(~tests::headless::)', '--message-format', 'libtest-json', '--no-tests', 'pass']
    if args.filter:
        command += ['--', args.filter]
    stdout, code = run(command, timeout=900, env=env, ok=None, with_code=True)
    tests = []
    for line in stdout.splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            print(line, file=sys.stderr)
            continue
        status = {'ok': 'passed', 'failed': 'failed', 'ignored': 'skipped', 'timeout': 'failed'}.get(item.get('event'))
        if item.get('type') == 'test' and status:
            tests.append({'name': item['name'], 'status': status})
            if item.get('stdout') and status == 'failed':
                print(item['stdout'], file=sys.stderr)
    tests.sort(key=lambda test: test['name'])
    if code and not tests:
        raise DebugError(3, f'nextest failed before reporting test results (exit {code})')
    if not tests:
        raise DebugError(2 if args.filter else 3, 'no GPUI headless tests matched')
    result = {'seed': seed, 'tests': tests, **{status: sum(test['status'] == status for test in tests)
                                           for status in ('passed', 'failed', 'skipped')}}
    return result, 3 if code or result['failed'] else 0


def artifact_dir(out):
    if out:
        out = Path(out).resolve()
        out.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix='ddg-profile-', dir=out))
    print(f'Profile artifacts: {directory}', file=sys.stderr)
    return directory


def lookup_binary(binary):
    path = Path(binary).resolve()
    if not path.is_file() and '/' not in binary and (found := shutil.which(binary)):
        path = Path(found).resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise DebugError(3, f'executable not found: {binary}')
    return str(path)


def profile(args):
    binary = lookup_binary(args.bin)
    if args.kind != 'cpu' and sys.platform != 'darwin':
        raise DebugError(4, 'xctrace recording requires macOS and Xcode')
    directory = artifact_dir(args.out)
    validation = METAL_ENV if args.metal_validation else {}
    if args.kind == 'cpu':
        path = directory / 'cpu.json'
        output = run(['samply', 'record', '--save-only', '--duration', str(args.duration),
                      '-o', str(path), '--', binary, *args.args], timeout=args.duration + 60,
                     env=dict(os.environ, **validation))
        if output:
            print(output, end='', file=sys.stderr)
        try:
            data = samply_summarize(json.loads(path.read_bytes()))
        except (ValueError, KeyError, TypeError, IndexError) as exc:
            raise DebugError(3, f'invalid samply profile: {exc}') from exc
        return {'kind': 'cpu', 'profile_path': str(path), **data}, 0

    trace = directory / f'{args.kind}.trace'
    env_flags = [word for key, value in validation.items() for word in ('--env', f'{key}={value}')]
    xctrace = ['xcrun', 'xctrace']
    output = run(xctrace + ['record', '--no-prompt', '--template', TEMPLATES[args.kind],
                            '--time-limit', f'{args.duration}s', '--output', str(trace), *env_flags,
                            '--launch', '--', binary, *args.args], timeout=args.duration + 90)
    if output:
        print(output, end='', file=sys.stderr)
    export = xctrace + ['export', '--input', str(trace)]
    toc_path = directory / 'toc.xml'
    run(export + ['--toc', '--output', str(toc_path)], timeout=60)
    try:
        toc = ET.parse(toc_path)
        # Pick one table per metric. Several Instruments schemas describe the same frames.
        available = {table.get('schema') for table in toc.findall('./run/data/table')}
        metrics = {'hitches': ('frames', 'hitches', 'samples'), 'metal': ('frames', 'samples'),
                   'alloc': ('allocations',), 'time': ('samples',)}[args.kind]
        schemas = [name for kind in metrics if (name := next(
            (name for name in TABLE_SCHEMAS[kind] if name in available), None))]
        queries = [(name, f'/trace-toc/run/data/table[@schema="{name}"]') for name in schemas]
        if args.kind == 'alloc' and not queries:
            track = 'run/tracks/track[@name="Allocations"]/details/detail'
            for label in ('Allocations List', 'Allocation Summary', 'Allocations Summary'):
                query = f'{track}[@name="{label}"]'
                if toc.find(query) is not None:
                    schema = label.lower().replace(' ', '-')
                    queries.append((schema, f'/trace-toc/{query}'))
                    schemas.append(schema)
                    break
        exports = []
        for schema, xpath in queries:
            xml = directory / f'{schema}.xml'
            run(export + ['--xpath', xpath, '--output', str(xml)], timeout=90)
            exports.append((xml.read_bytes(), schema))
        data = summarize_exports(exports)
    except (ValueError, ET.ParseError) as exc:
        raise DebugError(3, f'invalid xctrace export: {exc}') from exc
    return {'kind': args.kind, 'trace_path': str(trace), 'template': TEMPLATES[args.kind],
            'unavailable_metrics': [kind for kind in metrics if not any(table_kind(name) == kind for name in schemas)],
            **data}, 0


def gpui_trace(args):
    target = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target'))
    if not target.is_absolute():
        target = ROOT / target
    bins = [target / flavor / 'ddg-agents-native' for flavor in ('release', 'debug')]
    binary = next((path for path in bins if path.is_file() and os.access(path, os.X_OK)), None)
    if binary is None:
        run(['cargo', 'build', '--locked', '-p', 'ddg-agents-native'], timeout=900)
        binary = bins[-1]
    args.bin = str(binary)
    if args.check_vt:
        output = run([args.bin, '--check-vt', *args.args], timeout=30,
                     env=dict(os.environ, **(METAL_ENV if args.metal_validation else {})))
        return {'target': args.bin, 'verified': True, 'output': output.strip()}, 0
    args.kind = args.template
    return profile(args)

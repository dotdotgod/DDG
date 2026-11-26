#!/usr/bin/env python3
"""Summarize xctrace export tables or a Valgrind XML log as JSON."""
import math
import sys
from collections import Counter
from pathlib import Path
from xml.etree import ElementTree as ET

sys.dont_write_bytecode = True
from debug_common import DebugError, Parser, emit, error

LEAKS = ('DefinitelyLost', 'IndirectlyLost', 'PossiblyLost', 'StillReachable', 'Suppressed')
TABLE_SCHEMAS = {
    'frames': ('hitches-frame-lifetimes', 'metal-frames', 'frames', 'frame-lifetimes',
               'core-animation-frames', 'displayed-surfaces-interval'),
    'hitches': ('hitches', 'animation-hitches'),
    'samples': ('time-profile', 'cpu-profile'),
    'allocations': ('allocations', 'allocation-events', 'allocations-list', 'allocation',
                    'allocation-summary', 'allocations-summary', 'vm-allocation'),
}


def valgrind_summarize(document):
    root = ET.fromstring(document)
    if root.tag != 'valgrindoutput':
        raise ValueError('not a Valgrind XML log')
    counts = {pair.findtext('unique'): int(pair.findtext('count', '0'))
              for pair in root.findall('./errorcounts/pair')}
    leaks = {name: {'bytes': 0, 'blocks': 0} for name in LEAKS}
    errors = []
    for item in root.findall('error'):
        kind, unique = item.findtext('kind'), item.findtext('unique')
        issue = {'kind': kind, 'count': counts.get(unique, 1),
                 'message': item.findtext('what') or item.findtext('xwhat/text')}
        category = (kind or '').removeprefix('Leak_')
        if category in leaks:
            for key in ('bytes', 'blocks'):
                leaks[category][key] += int(item.findtext(f'xwhat/leaked{key}', '0'))
        errors.append(issue)
    # Match Memcheck's default error leak kinds; still report all requested loss records.
    non_errors = {'Leak_IndirectlyLost', 'Leak_StillReachable', 'Leak_Suppressed'}
    return {'error_count': sum(issue['count'] for issue in errors if issue['kind'] not in non_errors),
            'errors': errors, 'leak_summary': leaks,
            'finished': root.findtext('status[last()]/state') == 'FINISHED'}


def table_kind(name):
    return next((kind for kind, names in TABLE_SCHEMAS.items() if name in names), None)


def top(counts):
    return [{'symbol': symbol, 'samples': count}
            for symbol, count in sorted(counts.items(), key=lambda item: (-item[1], item[0]))[:10]]


def latency(values):
    values.sort()
    return {f'p{q}': round(values[math.ceil(len(values) * q / 100) - 1] / 1_000_000, 6)
            for q in (50, 95, 99)} if values else None


def rows(node):
    ids = {elem.attrib['id']: elem for elem in node.iter() if 'id' in elem.attrib}

    def resolved(elem):
        return ids.get(elem.get('ref'), elem)

    names = [col.findtext('mnemonic') for col in node.findall('./schema/col')]
    for row in node.findall('row'):
        cells = [resolved(elem) for elem in row]
        yield {name: cell for cell, name in zip(cells, names)}, cells, resolved


def number(elem):
    try:
        return int(elem.text) if elem is not None else None
    except (TypeError, ValueError):
        return None


def xctrace_summarize(document, schema_hint=None):
    return summarize_exports([(document, schema_hint)])


def summarize_exports(exports):
    nodes = []
    for document, schema_hint in exports:
        root = ET.fromstring(document)
        if root.tag != 'trace-query-result':
            raise ValueError('not an xctrace table export')
        nodes.extend((node, schema_hint) for node in root.findall('node'))
    tables, totals, frames = [], Counter(), Counter()
    durations = {'frames': [], 'hitches': []}
    allocated_bytes = 0
    for node, schema_hint in nodes:
        schema = node.find('./schema')
        name = schema.get('name', schema_hint or '') if schema is not None else schema_hint or ''
        kind = table_kind(name) or table_kind(schema_hint)
        count = 0
        for columns, cells, resolved in rows(node):
            count += 1
            if kind in durations:
                duration = next((value for col, elem in columns.items()
                                 if col and ('duration' in col or col in ('hitch-time', 'frame-time'))
                                 and (value := number(elem)) is not None), None)
                if duration is None:
                    duration = next((number(elem) for elem in cells if elem.tag == 'duration'), None)
                if duration is not None and duration >= 0:
                    durations[kind].append(duration)
            elif kind == 'samples':
                stack = next((elem for elem in cells if elem.tag in ('backtrace', 'tagged-backtrace')), None)
                frame = stack.find('.//frame') if stack is not None else None
                if frame is not None and (symbol := resolved(frame).get('name')):
                    frames[symbol] += 1
            elif kind == 'allocations':
                keys = ('size', 'bytes', 'allocation-size', 'total-bytes', 'persistent-bytes')
                sizes = [columns.get(key) for key in keys] + [cell for cell in cells if cell.tag in keys]
                allocated_bytes += next((size for cell in sizes if (size := number(cell)) is not None), 0)
        tables.append({'schema': name, 'rows': count})
        if kind:
            totals[kind] += count
    return {'tables': tables, 'frame_count': totals['frames'], 'hitch_count': totals['hitches'],
            'frame_duration_ms': latency(durations['frames']), 'hitch_duration_ms': latency(durations['hitches']),
            'sample_count': totals['samples'], 'top_frames': top(frames),
            'allocation_rows': totals['allocations'], 'allocation_bytes': allocated_bytes}


def samply_summarize(profile):
    threads, frames = [], Counter()
    for thread in profile['threads']:
        samples = thread['samples']
        threads.append({'name': thread['name'], 'pid': thread.get('pid'), 'tid': thread.get('tid'),
                        'samples': samples['length']})
        for stack in samples['stack']:
            if stack is not None:
                frame = thread['stackTable']['frame'][stack]
                func = thread['frameTable']['func'][frame]
                name = thread['funcTable']['name'][func]
                frames[thread['stringArray'][name]] += 1
    return {'sample_count': sum(thread['samples'] for thread in threads),
            'threads': threads, 'top_frames': top(frames)}


def main(argv):
    parser = Parser(description=__doc__)
    parser.add_argument('path', type=Path)
    parser.add_argument('--kind', choices=('xctrace', 'valgrind'), default='xctrace')
    parser.add_argument('--schema', help='schema name for an xctrace export that has no schema node')
    try:
        args = parser.parse_args(argv)
        document = args.path.read_bytes()
        emit(valgrind_summarize(document) if args.kind == 'valgrind'
             else xctrace_summarize(document, args.schema))
        return 0
    except (OSError, ValueError, ET.ParseError) as exc:
        return error(DebugError(3, str(exc)), True)
    except DebugError as exc:
        return error(exc, True)


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))

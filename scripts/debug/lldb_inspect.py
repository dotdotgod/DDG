#!/usr/bin/env python3
"""Single-stop LLDB snapshot. Run with Python 3; Xcode supplies the worker bindings."""
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True
from debug_common import DebugError, Parser, bounded, emit, error, run

ADDRESS = re.compile(r'0[xX][0-9a-fA-F]+\Z')
OS_CHOICES = ('macos', 'linux-arm64', 'linux-amd64')


def options(parser):
    parser.add_argument('--bin', required=True)
    parser.add_argument('--args', nargs='*', default=[], metavar='ARG',
                        help='debuggee arguments; put --args -- last for raw flags')
    parser.add_argument('--break', dest='breakpoint', required=True)
    parser.add_argument('--watch')
    parser.add_argument('--watch-size', type=int, choices=(1, 2, 4, 8), default=4)
    parser.add_argument('--mem', action='append', type=memory_arg, default=[], metavar='EXPR:LEN')
    parser.add_argument('--asm-count', type=bounded(0, 4096), default=32)
    parser.add_argument('--step-inst', type=bounded(0, 4096), default=0)
    parser.add_argument('--timeout', type=bounded(1, 86400), default=30)
    parser.add_argument('--remote', type=remote_arg, metavar='HOST:PORT',
                        help='Linux lldb-server platform endpoint')
    parser.add_argument('--os', choices=OS_CHOICES)


def memory_arg(value):
    expr, _, size = value.rpartition(':')
    if not expr.strip():
        raise ValueError('expected EXPR:LEN')
    return expr, bounded(1, 1024 * 1024)(size)


def remote_arg(value):
    if not re.fullmatch(r'(?:[\w.-]+|\[[a-fA-F0-9:.]+\]):[0-9]{1,5}', value):
        raise ValueError('expected HOST:PORT')
    bounded(1, 65535)(value.rsplit(':', 1)[1])
    return value


def parse(parser, argv, passthrough):
    if '--' in argv:
        end = argv.index('--')
        if end and argv[end - 1] == passthrough:
            args = parser.parse_args(argv[:end] + ['__ddg_passthrough__'])
            setattr(args, passthrough[2:], argv[end + 1:])
            return args
    return parser.parse_args(argv)


def inspect(args):
    if args.remote and args.os == 'macos':
        raise DebugError(2, '--remote expects a Linux lldb-server platform')
    if sys.platform == 'darwin' and args.os and args.os.startswith('linux-') and not args.remote:
        raise DebugError(2, 'Linux inspection from macOS requires --remote HOST:PORT')
    binary = Path(args.bin).resolve()
    if not binary.is_file() and '/' not in args.bin and (found := shutil.which(args.bin)):
        binary = Path(found).resolve()
    if not binary.is_file():
        raise DebugError(3, f'binary not found: {binary}')
    args.bin = str(binary)
    mac = sys.platform == 'darwin'
    deadline = time.monotonic() + args.timeout
    path = run((['xcrun'] if mac else []) + ['lldb', '-P'], timeout=args.timeout).strip()
    if not Path(path).is_dir():
        raise DebugError(4, f'LLDB Python bindings not found: {path}')
    env = dict(os.environ, PYTHONPATH=path)
    args.budget = deadline - time.monotonic()
    if args.budget <= 0:
        raise DebugError(124, f'LLDB inspection timed out after {args.timeout}s')
    try:
        result = run(['/usr/bin/python3' if mac else sys.executable, __file__,
                      '--worker', json.dumps(vars(args))], timeout=args.budget + 3,
                     env=env, ok=(0, 2, 3, 4, 124))
        data = json.loads(result)
    except json.JSONDecodeError as exc:
        raise DebugError(3, 'LLDB did not return a JSON snapshot') from exc
    if 'error' in data:
        raise DebugError(data['code'], data['error'])
    return data


def check(status, action):
    if status.Fail():
        raise DebugError(3, f'{action}: {status.GetCString()}')


def breakpoint(target, spec):
    if ADDRESS.fullmatch(spec):
        addr = int(spec, 16)
        if addr >= 1 << (8 * target.GetAddressByteSize()):
            raise DebugError(2, f'breakpoint address outside target address space: {spec}')
        file_address = target.ResolveFileAddress(addr)
        if file_address.IsValid():
            return target.BreakpointCreateBySBAddress(file_address)
        return target.BreakpointCreateByAddress(addr)
    file, sep, line = spec.rpartition(':')
    if file and sep and re.fullmatch(r'[0-9]+', line):
        if not 1 <= int(line) <= 2**32 - 1:
            raise DebugError(2, 'breakpoint line must be positive and fit in 32 bits')
        return target.BreakpointCreateByLocation(file, int(line))
    return target.BreakpointCreateByName(spec)


def location(frame):
    entry = frame.GetLineEntry()
    return {'file': entry.GetFileSpec().fullpath if entry.IsValid() else None,
            'line': entry.GetLine() if entry.IsValid() else None,
            'symbol': frame.GetDisplayFunctionName() or frame.GetFunctionName()}


def registers(frame):
    return {reg.GetName(): reg.GetValue() for bank in frame.GetRegisters()
            for reg in bank if reg.GetValue() is not None}


def address(lldb, frame, expr, args):
    if ADDRESS.fullmatch(expr):
        value = int(expr, 16)
    else:
        options = lldb.SBExpressionOptions()
        options.SetIgnoreBreakpoints(True)
        options.SetUnwindOnError(True)
        options.SetTryAllThreads(False)
        options.SetTimeoutInMicroSeconds(min(2**32 - 1, max(1, int(remaining(args) * 1_000_000))))
        result = frame.EvaluateExpression(expr, options)
        remaining(args)
        check(result.GetError(), f'evaluate {expr!r}')
        status = lldb.SBError()
        value = result.GetValueAsUnsigned(status, 0)
        check(status, f'convert {expr!r} to address')
    if not 0 <= value < 1 << (8 * frame.GetThread().GetProcess().GetAddressByteSize()):
        raise DebugError(2, f'address outside target address space: {expr}')
    return value


def memory(lldb, process, frame, args):
    result = []
    for expr, size in args.mem:
        remaining(args)
        base, status = address(lldb, frame, expr, args), lldb.SBError()
        raw = process.ReadMemory(base, size, status)
        check(status, f'read {expr!r} at {base:#x}')
        if len(raw) != size:
            raise DebugError(3, f'short memory read at {base:#x}: {len(raw)} of {size} bytes')
        result.append({'expr': expr, 'addr': hex(base), 'size': size, 'hex': raw.hex(),
                       'ascii': ''.join(chr(byte) if 32 <= byte < 127 else '.' for byte in raw)})
    return result


def disassemble(lldb, target, frame, count):
    result = []
    if count == 0:
        return result
    start = frame.GetPCAddress()
    # Decode from a known instruction boundary on variable-width architectures.
    function = frame.GetFunction()
    if not function.IsValid():
        function = frame.GetSymbol()
    if count >= 4 and function.IsValid():
        decoded = function.GetInstructions(target)
        for i, inst in enumerate(decoded):
            if inst.GetAddress().GetLoadAddress(target) == frame.GetPC():
                start = decoded.GetInstructionAtIndex(max(0, i - count // 4)).GetAddress()
                break
    for inst in target.ReadInstructions(start, count):
        data = inst.GetData(target)
        raw = data.ReadRawData(lldb.SBError(), 0, data.GetByteSize())
        result.append({'addr': hex(inst.GetAddress().GetLoadAddress(target)),
                       'bytes': raw.hex(' '), 'mnemonic': inst.GetMnemonic(target),
                       'operands': inst.GetOperands(target)})
    return result


def stopped(lldb, process, reason=None, identifier=None):
    if process.GetState() == lldb.eStateStopped:
        for thread in process:
            if reason is None or (thread.GetStopReason() == reason and
                    identifier in [thread.GetStopReasonDataAtIndex(i) for i in range(
                        0, thread.GetStopReasonDataCount(),
                        2 if reason == lldb.eStopReasonBreakpoint else 1)]):
                return thread if reason is not None else process.GetSelectedThread()
        thread = process.GetSelectedThread()
        detail = thread.GetStopDescription(256) if thread.IsValid() else 'unknown stop'
    elif process.GetState() == lldb.eStateExited:
        detail = f'exited with status {process.GetExitStatus()} ({process.GetExitDescription() or "no detail"})'
    else:
        detail = lldb.SBDebugger.StateAsCString(process.GetState())
    raise DebugError(3, f'target did not reach the requested stop: {detail}')


def remaining(args):
    seconds = args.deadline - time.monotonic()
    if seconds <= 0:
        raise DebugError(124, f'LLDB inspection timed out after {args.timeout}s')
    return seconds


def wait_for_stop(lldb, process, listener, args, previous=0):
    event = lldb.SBEvent()
    while True:
        seconds = remaining(args)
        while listener.GetNextEvent(event):
            if lldb.SBProcess.EventIsProcessEvent(event):
                state = lldb.SBProcess.GetStateFromEvent(event)
                if state in (lldb.eStateExited, lldb.eStateCrashed, lldb.eStateDetached):
                    return
                if (state == lldb.eStateStopped and not lldb.SBProcess.GetRestartedFromEvent(event)
                        and process.GetStopID() > previous):
                    return
        time.sleep(min(0.01, seconds))


def snapshot(lldb, debugger, args):
    debugger.SetAsync(True)
    listener = debugger.GetListener()
    status = lldb.SBError()
    if args.remote:
        platform = lldb.SBPlatform('remote-linux')
        check(platform.ConnectRemote(lldb.SBPlatformConnectOptions('connect://' + args.remote)),
              'connect to lldb-server platform')
        debugger.SetSelectedPlatform(platform)
    arch = {'linux-arm64': 'aarch64-unknown-linux-gnu',
            'linux-amd64': 'x86_64-unknown-linux-gnu'}.get(args.os)
    target = debugger.CreateTarget(args.bin, arch, None, True, status)
    check(status, 'create target')
    bp = breakpoint(target, args.breakpoint)
    if not bp.IsValid() or not bp.GetNumLocations():
        raise DebugError(3, f'unresolved breakpoint: {args.breakpoint}')
    launch = target.GetLaunchInfo()
    launch.SetArguments(args.args, False)
    launch.SetLaunchFlags(launch.GetLaunchFlags() & ~lldb.eLaunchFlagDisableASLR)
    launch.AddOpenFileAction(0, os.devnull, True, False)
    launch.AddOpenFileAction(1, os.devnull, False, True)
    launch.AddOpenFileAction(2, os.devnull, False, True)
    process = target.Launch(launch, status)
    try:
        check(status, 'launch target')
        wait_for_stop(lldb, process, listener, args)
        thread = stopped(lldb, process, lldb.eStopReasonBreakpoint, bp.GetID())
        if args.watch:
            base = address(lldb, thread.GetFrameAtIndex(0), args.watch, args)
            wp = target.WatchAddress(base, args.watch_size, False, True, status)
            check(status, f'watch {args.watch!r}')
            bp.SetEnabled(False)
            previous = process.GetStopID()
            check(process.Continue(), 'continue to watchpoint')
            wait_for_stop(lldb, process, listener, args, previous)
            thread = stopped(lldb, process, lldb.eStopReasonWatchpoint, wp.GetID())
        frame = thread.GetFrameAtIndex(0)
        reason = thread.GetStopDescription(256)
        trace = []
        regs, mem = registers(frame), memory(lldb, process, frame, args)
        for i in range(args.step_inst):
            before = frame.GetPC()
            previous = process.GetStopID()
            thread_id = thread.GetThreadID()
            process.SetSelectedThread(thread)
            try:
                thread.StepInstruction(False, status)
            except TypeError:  # LLDB 14 only accepts the step-over flag.
                thread.StepInstruction(False)
            else:
                check(status, 'step instruction')
            wait_for_stop(lldb, process, listener, args, previous)
            stopped(lldb, process)
            thread = process.GetThreadByID(thread_id)
            if not thread.IsValid() or thread.GetStopReason() in (lldb.eStopReasonInvalid, lldb.eStopReasonNone):
                raise DebugError(3, 'instruction step interrupted before the selected thread stopped')
            frame = thread.GetFrameAtIndex(0)
            next_regs, next_mem = registers(frame), memory(lldb, process, frame, args)
            trace.append({'step': i + 1, 'from_pc': hex(before), 'pc': hex(frame.GetPC()),
                          'stop_reason': thread.GetStopDescription(256),
                          'registers': {key: {'before': regs.get(key), 'after': next_regs.get(key)}
                                        for key in sorted(regs.keys() | next_regs.keys())
                                        if regs.get(key) != next_regs.get(key)},
                          'memory': [{'before': old, 'after': new} for old, new in zip(mem, next_mem)
                                     if old != new]})
            regs, mem = next_regs, next_mem
        return {'target': args.bin, 'triple': target.GetTriple(), 'stop_reason': reason,
                'location': location(frame), 'pc': hex(frame.GetPC()), 'registers': regs,
                'disassembly': disassemble(lldb, target, frame, args.asm_count), 'memory': mem,
                'step_trace': trace, 'backtrace': [{'index': i, **location(f)} for i, f in enumerate(thread)]}
    finally:
        if process.IsValid() and process.GetState() not in (lldb.eStateExited, lldb.eStateDetached):
            status = process.Kill()
            if status.Fail():
                print(f'kill debuggee: {status.GetCString()}', file=sys.stderr)


def worker(args):
    # Xcode's Python and the CLI's Python can use different monotonic clock origins.
    args.deadline = time.monotonic() + args.budget
    # Native debugger and debuggee output must never share the protocol descriptor.
    with os.fdopen(os.dup(1), 'w') as output:
        os.dup2(2, 1)
        try:
            import lldb
        except ImportError as exc:
            print(json.dumps({'code': 4, 'error': f'cannot import LLDB bindings: {exc}'}), file=output)
            return 4
        lldb.SBDebugger.Initialize()
        debugger = lldb.SBDebugger.Create(False)
        code = 0
        try:
            payload = snapshot(lldb, debugger, args)
        except DebugError as exc:
            code, payload = exc.code, {'code': exc.code, 'error': str(exc)}
        finally:
            lldb.SBDebugger.Destroy(debugger)
            lldb.SBDebugger.Terminate()
        print(json.dumps(payload, allow_nan=False), file=output)
        return code


def main(argv):
    if len(argv) == 2 and argv[0] == '--worker':
        from argparse import Namespace
        return worker(Namespace(**json.loads(argv[1])))
    parser = Parser(description=__doc__)
    options(parser)
    try:
        emit(inspect(parse(parser, argv, '--args')))
        return 0
    except (DebugError, OSError, subprocess.TimeoutExpired) as exc:
        return error(exc, True)


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))

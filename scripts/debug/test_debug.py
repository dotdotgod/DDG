"""Run with python3 scripts/debug/test_debug.py (runtime check uses Xcode on macOS)."""
import json
import os
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CLI = Path(__file__).resolve().parents[1] / 'ddg-debug'
parse_layout = runpy.run_path(str(CLI))['parse_layout']
from xctrace_summarize import valgrind_summarize, xctrace_summarize


def cli(*args, env=None):
    proc = subprocess.run([sys.executable, str(CLI), '--json', *args], capture_output=True,
                          text=True, timeout=25, env=env)
    return proc.returncode, json.loads(proc.stdout)


class DebugTests(unittest.TestCase):
    def test_layout_offsets(self):
        frame, enum = parse_layout('''print-type-size type: `Frame`: 8 bytes, alignment: 4 bytes
print-type-size     field `.word`: 4 bytes
print-type-size     field `.kind`: 1 bytes
print-type-size     end padding: 3 bytes
print-type-size type: `FrameOrEmpty`: 12 bytes, alignment: 4 bytes
print-type-size     discriminant: 1 bytes
print-type-size     variant `Frame`: 11 bytes
print-type-size         padding: 3 bytes
print-type-size         field `.0`: 8 bytes, alignment: 4 bytes
print-type-size     variant `Empty`: 0 bytes
''', 'Frame')
        self.assertEqual(frame['fields'][1]['offset'], 4)
        self.assertEqual(frame['padding'], [{'offset': 5, 'size': 3}])
        self.assertEqual(enum['variants'][0]['fields'][0]['offset'], 4)
        self.assertEqual(enum['variants'][1]['fields'], [])

    def test_agent_exit_codes(self):
        self.assertEqual(cli('inspect', '--bin', 'unused', '--break', 'main', '--mem', 'bad')[0], 2)
        self.assertEqual(cli('inspect', '--bin', '/nonexistent/ddg-probe', '--break', 'main')[0], 3)
        self.assertEqual(cli('linux-up')[0], 2)
        self.assertEqual(cli('gpui-test', '--seed', str(2**64))[0], 2)
        self.assertEqual(cli('mem', '--os', 'linux-arm64', '--tool', 'valgrind', '--pid', '1')[0], 2)
        code, error = cli('linux-up', '--arch', 'arm64', env=dict(os.environ, PATH='/nonexistent'))
        self.assertEqual((code, error['code']), (4, 4))

    def test_xctrace_hitches_and_sample_references(self):
        data = xctrace_summarize('''<?xml version="1.0"?>
<trace-query-result>
 <node xpath="//trace-toc[1]/run[1]/data[1]/table[38]">
  <schema name="hitches"><col><mnemonic>start</mnemonic></col>
    <col><mnemonic>duration</mnemonic></col></schema>
  <row><start-time id="1">1243418291</start-time><duration id="2" fmt="16 ms">16000000</duration></row>
  <row><start-time id="3">1274400000</start-time><duration id="4" fmt="33 ms">33000000</duration></row>
  <row><start-time id="5">1307400000</start-time><duration ref="4"/></row>
 </node>
 <node xpath="//trace-toc[1]/run[1]/data[1]/table[14]">
  <schema name="time-profile"><col><mnemonic>time</mnemonic></col><col><mnemonic>stack</mnemonic></col></schema>
  <row><sample-time id="1">1242417166</sample-time><tagged-backtrace id="2">
    <frame id="3" name="draw_frame" addr="0x100004a10"/><frame id="4" name="main"/>
  </tagged-backtrace></row>
  <row><sample-time id="5">1243418291</sample-time><tagged-backtrace ref="2"/></row>
 </node>
 <node xpath="//trace-toc[1]/run[1]/data[1]/table[45]">
   <schema name="allocations"><col><mnemonic>size</mnemonic></col></schema>
   <row><uint64 id="1">4096</uint64></row>
 </node>
</trace-query-result>''')
        self.assertEqual((data['hitch_count'], data['sample_count'], data['allocation_bytes']), (3, 2, 4096))
        self.assertEqual(data['hitch_duration_ms'], {'p50': 33.0, 'p95': 33.0, 'p99': 33.0})
        self.assertEqual(data['top_frames'], [{'symbol': 'draw_frame', 'samples': 2}])
        frames = xctrace_summarize('''<trace-query-result><node><row><start-time>1000000</start-time>
          <duration fmt="12 ms">12000000</duration></row></node></trace-query-result>''',
          'hitches-frame-lifetimes')
        self.assertEqual((frames['frame_count'], frames['frame_duration_ms']['p95']), (1, 12))

    def test_valgrind_xml(self):
        data = valgrind_summarize('''<?xml version="1.0"?>
<valgrindoutput>
 <protocolversion>4</protocolversion><protocoltool>memcheck</protocoltool>
 <status><state>RUNNING</state></status>
 <error><unique>0x0</unique><tid>1</tid><kind>InvalidRead</kind><what>Invalid read of size 4</what></error>
 <error><unique>0x1</unique><tid>1</tid><kind>Leak_DefinitelyLost</kind><xwhat>
   <text>128 bytes in 2 blocks are definitely lost</text><leakedbytes>128</leakedbytes><leakedblocks>2</leakedblocks>
 </xwhat></error>
 <status><state>FINISHED</state></status>
 <errorcounts><pair><count>3</count><unique>0x0</unique></pair></errorcounts>
</valgrindoutput>''')
        self.assertEqual(data['error_count'], 4)
        self.assertEqual(data['errors'][0]['message'], 'Invalid read of size 4')
        self.assertEqual(data['leak_summary']['DefinitelyLost'], {'bytes': 128, 'blocks': 2})
        self.assertTrue(data['finished'])

    @unittest.skipUnless(sys.platform == 'darwin' and shutil.which('xcrun'), 'requires Xcode')
    def test_lldb_watch_step_and_timeout(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            source, binary, pidfile = (work / name for name in ('probe.c', 'probe', 'pid'))
            source.write_text('''#include <stdio.h>
#include <unistd.h>
volatile int watched;
void tick(void) { watched = 7; }
void never_called(void) {}
int main(int n, char **a) {
    if (n == 2) {
        FILE *out = fopen(a[1], "w");
        fprintf(out, "%d\\n", getpid());
        fclose(out);
        for (;;) sleep(1);
    }
    tick();
    return 0;
}
''')
            subprocess.run(['xcrun', 'clang', '-g', '-O0', str(source), '-o', str(binary)],
                           check=True, timeout=15)
            symbols = subprocess.check_output(['xcrun', 'nm', str(binary)], text=True, timeout=10)
            tick = next(line.split()[0] for line in symbols.splitlines() if line.endswith(' _tick'))
            code, result = cli('inspect', '--bin', str(binary), '--break', f'0x{tick}',
                               '--watch', '&watched', '--mem', '&watched:4', '--step-inst', '2')
            self.assertEqual(code, 0, result)
            self.assertIn('watchpoint', result['stop_reason'])
            self.assertEqual(result['memory'][0]['hex'], '07000000')
            self.assertTrue(result['registers'] and result['disassembly'] and result['backtrace'])
            self.assertEqual(len(result['step_trace']), 2)
            code, result = cli('inspect', '--bin', str(binary), '--break', 'never_called',
                               '--timeout', '3', '--args', str(pidfile))
            self.assertEqual((code, result['code']), (124, 124))
            pid = int(pidfile.read_text())
            try:
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
            finally:
                try:
                    os.kill(pid, 9)
                except ProcessLookupError:
                    pass


if __name__ == '__main__':
    unittest.main()

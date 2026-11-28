import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile


class ReleasePlatforms(unittest.TestCase):
    def test_require_all_includes_intel_desktop(self):
        headless = ['ddg', 'ddg-agents-host', 'ddg-terminal-server']
        desktop = [*headless, 'ddg-agents-native']
        platforms = ['linux-x86_64', 'linux-aarch64', 'windows-x86_64', 'darwin-arm64', 'darwin-x86_64']
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            command = [sys.executable, Path(__file__).with_name('generate-manifest.py'),
                       '--artifacts-dir', tmp, '--tag', 'v1.2.3', '--require-all']
            macos = 'DDG Agents.app/Contents/'
            for platform in platforms:
                base = root / f'ddg-agents-1.2.3-{platform}'
                if platform == 'darwin-x86_64':
                    missing = subprocess.run(command, capture_output=True, text=True)
                    self.assertNotEqual(missing.returncode, 0)
                    self.assertIn(f'{base.name}.app.zip', missing.stderr)
                    self.assertFalse((root / 'manifest.json').exists())
                if platform.startswith('linux-'):
                    with tarfile.open(f'{base}.tar.gz', 'w:gz') as archive:
                        for name in [*headless, 'THIRD_PARTY_NOTICES.txt']:
                            member = tarfile.TarInfo(name)
                            member.size = 1
                            archive.addfile(member, io.BytesIO(b'x'))
                else:
                    mac = platform.startswith('darwin-')
                    with zipfile.ZipFile(f'{base}.{"app.zip" if mac else "zip"}', 'w') as archive:
                        for name in desktop:
                            archive.writestr(macos + 'MacOS/' + name if mac else name + '.exe', b'x')
                        archive.writestr((macos + 'Resources/' if mac else '') + 'THIRD_PARTY_NOTICES.txt', b'x')
            subprocess.run(command, check=True)
            assets = {a['platform']: a for a in json.loads((root / 'manifest.json').read_bytes())['artifacts']}
            self.assertEqual(set(assets), set(platforms))
            intel = assets['darwin-x86_64']
            self.assertEqual(intel['name'], 'ddg-agents-1.2.3-darwin-x86_64.app.zip')
            self.assertEqual({k: v['path'] for k, v in intel['binaries'].items()},
                             {name: macos + 'MacOS/' + name for name in desktop})

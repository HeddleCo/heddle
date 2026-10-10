# SPDX-License-Identifier: Apache-2.0
"""Synthetic regression of helper seams; actual agent activity is separate evidence."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tarfile
import unittest
from gateway.core import git_env
from gateway.pack_demo import pack
from gateway.smoke import run as smoke


class AgentDemoTests(unittest.TestCase):
    def test_real_capture_annotation_and_reviewed_signed_integration(self):
        native = Path(os.environ['GATEWAY_NATIVE']).resolve()
        helper = Path(os.environ.get('GATEWAY_AGENT_DEMO', native.with_name('gateway_agent_demo')))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'native').mkdir()
            (root / 'native' / 'app.js').write_text('export const features = [];\n')
            env = git_env()
            env.update(HEDDLE_HOME=str(root / 'identity'), HEDDLE_PRINCIPAL_NAME='Synthetic Test Agent',
                HEDDLE_PRINCIPAL_EMAIL='fixture@example.invalid', HEDDLE_AGENT_PROVIDER='test-fixture',
                HEDDLE_AGENT_MODEL='not-a-real-model', HEDDLE_SESSION_ID='helper-regression', HEDDLE_SESSION_SEGMENT='unit-test')
            def call(*args):
                result = subprocess.run([str(helper), *args], env=env, capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stderr)
                return json.loads(result.stdout)
            baseline = call('prepare', str(root))
            states = []
            for name, checkout, feature in [('demo/open-filter', 'agent-filter', 'filter'),
                                             ('demo/priority-sort', 'agent-sort', 'sort')]:
                (root / checkout / 'app.js').write_text(f'export const features = ["{feature}"];\n')
                captured = call('capture', str(root), name)
                self.assertIsNotNone(captured['attribution']['agent'])
                states.append(captured['state'])
                note = root / (feature + '.txt')
                note.write_text(feature + ' invariant')
                call('annotate', str(root), name, str(note))
            combined = root / 'combined'
            combined.mkdir()
            (combined / 'app.js').write_text('export const features = ["filter", "sort"];\n')
            integrated = call('integrate-reviewed', str(root), str(combined))
            self.assertEqual(integrated['parents'], [states[1], states[0]])
            inspected = call('inspect', str(root), integrated['state'])
            self.assertEqual(len(inspected['context']), 1)
            self.assertEqual(len(inspected['context'][0]['blob']['annotations']), 3)
            grants = json.dumps(['main', 'demo/open-filter', 'demo/priority-sort'])
            result = subprocess.run([str(native), 'export', str(root / 'native'), integrated['state'],
                str(root / 'view.git'), 'snapshot', 'demo/priority-sort', grants],
                env=env, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertRegex(result.stdout.strip(), r'^[0-9a-f]{40}$')
            denied = subprocess.run([str(native), 'export', str(root / 'native'), integrated['state'],
                str(root / 'wrong-main.git'), 'snapshot', 'main', grants],
                env=env, capture_output=True, text=True, timeout=30)
            self.assertNotEqual(denied.returncode, 0)
            self.assertIn('no admitted native original', denied.stderr)
            self.assertNotEqual(baseline['base'], integrated['state'])
            dataset = root / 'dataset'
            metadata = pack(native, root / 'native', baseline['base'], integrated['state'], dataset,
                threads=['main', 'demo/open-filter', 'demo/priority-sort'])
            self.assertFalse(metadata['authority_included'])
            self.assertTrue(metadata['synthetic_only'])
            with tarfile.open(dataset / 'native.bundle', 'r:') as archive:
                names = archive.getnames()
            for forbidden in ['identity.toml', 'metadata.sqlite3-wal', 'metadata.sqlite3-shm',
                    'thread_workspaces', 'thread_records', 'oplog']:
                self.assertFalse(any(forbidden in name.split('/') for name in names), forbidden)
            replay = smoke(native, root / 'replay', dataset)
            self.assertEqual(replay['status'], 'passed')
            self.assertEqual(replay['views'], metadata['views'])

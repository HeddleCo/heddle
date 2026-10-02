# SPDX-License-Identifier: Apache-2.0
"""Offline contract fixtures, never claimed as real provider emissions."""
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import tempfile
import unittest

from collector import Collector, MAX_BYTES, MAX_OPERATIONS, Sink, bounded_paths
from claude import receive


def message(tool='call', model='response-model', session='session'):
    return {'type': 'assistant', 'session_id': session, 'request_id': 'request',
            'message': {'id': 'response', 'model': model, 'content': [
                {'type': 'text', 'text': 'PRIVATE BODY'},
                {'type': 'tool_use', 'id': tool, 'input': {'secret': 'PRIVATE BODY'}}]}}


def hook(phase='PreToolUse', tool='call'):
    return {'hook_event_name': phase, 'session_id': 'session', 'tool_use_id': tool,
            'tool_name': 'Edit', 'tool_input': {'file_path': 'file', 'new_string': 'PRIVATE BODY'}}


class Adapters(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.events = []
        self.collector = Collector('claude-code', '2.1.287', self.root, self.events.append)

    def test_input_bounds_reject_oversize_and_non_object_frames(self):
        with self.assertRaises(ValueError):
            receive(io.BytesIO(b'x' * (MAX_BYTES + 1) + b'\n'))
        with self.assertRaises(ValueError):
            receive(io.BytesIO(b'[]\n'))
        self.collector.claude_message(message(model='x' * 257))
        self.collector.hook(hook())
        self.assertEqual(len(self.events), 1)
        self.assertFalse(self.events[0]['identity']['response'])

    def test_message_before_hook_and_privacy(self):
        self.collector.claude_message(message())
        self.collector.hook(hook())
        self.assertEqual([e['phase'] for e in self.events], ['before', 'observe'])
        self.assertEqual(self.events[1]['identity']['response']['model']['value'], 'response-model')
        self.assertFalse(self.events[1]['identity']['selected'])
        self.assertEqual(self.events[1]['identity']['harness_version']['source'], 'process')
        self.assertNotIn('PRIVATE BODY', json.dumps(self.events))

    def test_late_metadata_does_not_repeat_file_phase(self):
        self.collector.hook(hook())
        self.collector.hook(hook('PostToolUse'))
        self.collector.claude_message(message())
        self.assertEqual([e['phase'] for e in self.events], ['before', 'after', 'observe'])
        self.assertFalse(self.events[-1]['paths'])

    def test_conflicts_stay_poisoned_and_duplicate_stream_is_idempotent(self):
        for model in ('one', 'two', 'one', 'three'):
            self.collector.claude_message(message(model=model))
        self.collector.hook(hook())
        models = [e['identity']['response']['model']['value'] for e in self.events[1:]]
        self.assertEqual(models, ['one', 'two'])

    def test_other_session_and_missing_tool_never_borrow_model(self):
        self.collector.claude_message(message(session='other'))
        self.collector.hook(hook())
        self.assertEqual(len(self.events), 1)
        self.assertFalse(self.events[0]['identity']['response'])

    def test_subagent_scope_is_hook_reported(self):
        self.collector.hook({**hook(), 'agent_id': 'child'})
        self.collector.claude_message({**message(), 'parent_tool_use_id': 'parent-tool'})
        scope = self.events[-1]['identity']['scope']
        self.assertEqual(scope['actor_id'], 'child')
        self.assertEqual(scope['parent_harness_session_id'], 'session')
        self.assertNotIn('parent_actor_id', scope)

    def test_bounds_unknown_version_and_paths(self):
        with self.assertRaises(ValueError):
            Collector('hermes', '0.22.0', self.root, self.events.append)
        for i in range(MAX_OPERATIONS + 10):
            self.collector.claude_message(message(tool=str(i)))
        self.assertEqual(len(self.collector.models), MAX_OPERATIONS)
        self.collector.hook(hook(tool='untracked'))
        self.assertEqual(len(self.events), 1)
        self.assertFalse(bounded_paths(self.root, 'Edit', {'file_path': '../outside'}, 'claude-code'))
        self.assertFalse(bounded_paths(self.root, 'Bash', {'file_path': 'file'}, 'claude-code'))
        (self.root / 'link').symlink_to(self.root.parent)
        self.assertFalse(bounded_paths(self.root, 'Edit', {'file_path': 'link/file'}, 'claude-code'))

    def test_hermes_paths_require_explicit_local_backend_and_absolute_path(self):
        event = {'event': 'pre_tool_call', 'session_id': 's', 'turn_id': 't',
                 'api_request_id': 'r', 'tool_call_id': 'c', 'tool_name': 'patch',
                 'args': {'path': str(self.root / 'file'), 'mode': 'replace'}}
        c = Collector('hermes', '0.21.0', self.root, self.events.append)
        c.hook(event)
        self.assertEqual(self.events[-1]['paths'], [])
        c = Collector('hermes', '0.21.0', self.root, self.events.append, local_files=True)
        c.hook(event)
        self.assertEqual(self.events[-1]['paths'], [str(self.root / 'file')])
        c.hook({**event, 'args': {'path': 'file'}})
        self.assertEqual(self.events[-1]['paths'], [])

    def test_hermes_actual_post_api_shape_and_retry_scope(self):
        c = Collector('hermes', '0.21.0', self.root, self.events.append)
        response = {'session_id': 's', 'turn_id': 't', 'api_request_id': 'api',
                    'model': 'selected-alias', 'provider': 'provider', 'response_model': 'response-model',
                    'response': {'assistant_message': {'content': 'PRIVATE BODY', 'tool_calls': [
                        {'id': 'c', 'function': {'arguments': 'PRIVATE BODY'}}]}}, 'base_url': 'PRIVATE BODY'}
        c.hook({**response, 'event': 'post_api_request'})
        tool = {'event': 'pre_tool_call', 'session_id': 's', 'turn_id': 't',
                'api_request_id': 'api', 'tool_call_id': 'c', 'tool_name': 'write_file',
                'args': {'path': 'file', 'content': 'PRIVATE BODY'}}
        c.hook(tool)
        joined = self.events[-1]['identity']
        self.assertEqual(joined['selected']['model']['value'], 'selected-alias')
        self.assertEqual(joined['response']['model']['value'], 'response-model')
        self.assertNotIn('attempt_id', joined['scope'])
        c.hook({**tool, 'event': 'post_tool_call', 'status': 'error'})
        self.assertEqual(self.events[-2]['phase'], 'failed')
        self.assertNotIn('PRIVATE BODY', json.dumps(self.events))
        c.hook({**tool, 'api_request_id': 'different'})
        self.assertFalse(self.events[-1]['identity']['response'])


@unittest.skipUnless(os.environ.get('HEDDLE_COLLECTOR_TEST_BINARY'), 'requires separately built Heddle')
class Journal(unittest.TestCase):
    def test_runner_forwards_explicit_prompt_and_invocation_hooks(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory).resolve()
            root = parent / 'repo'
            root.mkdir()
            binary = os.environ['HEDDLE_COLLECTOR_TEST_BINARY']
            def cli(*args):
                p = subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, timeout=30)
                self.assertEqual(p.returncode, 0, p.stderr)
                return json.loads(p.stdout)
            cli('init', '--no-harness-install', '--principal-name', 'Fixture',
                '--principal-email', 'fixture@example.invalid', '--output', 'json')
            (root / 'file').write_text('before')
            cli('capture', '-m', 'baseline', '--no-agent', '--output', 'json')
            fake = parent / 'claude-fixture'
            fake.write_text('#!' + sys.executable + "\n" + """
import sys,json,subprocess,shlex
from pathlib import Path
if '--version' in sys.argv:
    print('2.1.287 (Claude Code)')
    sys.exit(0)
assert sys.argv[sys.argv.index('-p') + 1] == 'synthetic task'
settings_path = Path(sys.argv[sys.argv.index('--settings') + 1])
Path('../settings-path').write_text(str(settings_path))
settings = json.loads(settings_path.read_text())
print(json.dumps({'type':'assistant','session_id':'s','message':{'id':'m','model':'response-model','content':[{'type':'tool_use','id':'c'}]}}), flush=True)
for event in ('PreToolUse','PostToolUse'):
    if event == 'PostToolUse': Path('file').write_text('after')
    command = settings['hooks'][event][0]['hooks'][0]['command']
    payload = {'hook_event_name':event,'session_id':'s','tool_use_id':'c','tool_name':'Edit','tool_input':{'file_path':str(Path('file').resolve())}}
    subprocess.run(shlex.split(command), input=json.dumps(payload).encode(), check=True)
""")
            fake.chmod(0o700)
            p = subprocess.run([sys.executable, str(Path(__file__).with_name('claude.py')),
                                '--repo', str(root), '--heddle', binary, '--claude', str(fake),
                                '--timeout', '10', '--prompt', 'synthetic task'], capture_output=True, timeout=20)
            self.assertEqual(p.returncode, 0, p.stderr)
            self.assertFalse(Path((parent / 'settings-path').read_text()).exists())
            cli('capture', '-m', 'fake harness boundary', '--output', 'json')
            ops = cli('show', '--output', 'json')['attribution_evidence']['operations']
            self.assertEqual(len(ops), 1)
            self.assertEqual(ops[0]['resolution'], 'content_bound')
            self.assertEqual(ops[0]['identity']['response']['model']['value'], 'response-model')

    def test_claude_runner_timeout_and_temporary_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake = root / 'claude-fixture'
            fake.write_text('#!' + sys.executable + '\nimport sys,time\nif "--version" in sys.argv: print("2.1.287 (Claude Code)")\nelse: time.sleep(30)\n')
            fake.chmod(0o700)
            start = time.monotonic()
            p = subprocess.run([sys.executable, str(Path(__file__).with_name('claude.py')),
                                '--repo', str(root), '--heddle', os.environ['HEDDLE_COLLECTOR_TEST_BINARY'],
                                '--claude', str(fake), '--timeout', '1', '--prompt', 'fixture'],
                               capture_output=True, timeout=10)
            self.assertEqual(p.returncode, 124, p.stderr)
            self.assertLess(time.monotonic() - start, 8)

    def test_hermes_two_response_models_survive_capture(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            binary = os.environ['HEDDLE_COLLECTOR_TEST_BINARY']
            def cli(*args):
                p = subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, timeout=30)
                self.assertEqual(p.returncode, 0, p.stderr)
                return json.loads(p.stdout)
            cli('init', '--no-harness-install', '--principal-name', 'Fixture',
                '--principal-email', 'fixture@example.invalid', '--output', 'json')
            (root / 'file').write_text('before')
            cli('capture', '-m', 'baseline', '--no-agent', '--output', 'json')
            c = Collector('hermes', '0.21.0', root, Sink(binary, root), local_files=True)
            for n in ('one', 'two'):
                common = {'session_id': 's', 'turn_id': 't-' + n, 'api_request_id': 'r-' + n}
                c.hook({**common, 'event': 'post_api_request', 'model': 'alias-' + n,
                        'response_model': 'response-' + n, 'provider': 'provider',
                        'response': {'assistant_message': {'tool_calls': [{'id': n}]}}})
                event = {**common, 'tool_call_id': n, 'tool_name': 'write_file',
                         'args': {'path': str(root / 'file')}}
                c.hook({**event, 'event': 'pre_tool_call'})
                (root / 'file').write_text(n)
                c.hook({**event, 'event': 'post_tool_call', 'status': 'ok'})
            cli('capture', '-m', 'Hermes offline hook fixture', '--output', 'json')
            ops = cli('show', '--output', 'json')['attribution_evidence']['operations']
            self.assertEqual(len(ops), 2)
            self.assertTrue(all(o['resolution'] == 'content_bound' for o in ops))
            self.assertEqual({o['identity']['response']['model']['value'] for o in ops},
                             {'response-one', 'response-two'})
            self.assertTrue(all(o['identity']['scope']['attempt_id'] is None for o in ops))

    def test_real_shared_boundary_late_join_conflict_capture(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            binary = os.environ['HEDDLE_COLLECTOR_TEST_BINARY']
            def cli(*args):
                p = subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, timeout=30)
                self.assertEqual(p.returncode, 0, p.stderr)
                return json.loads(p.stdout)
            cli('init', '--no-harness-install', '--principal-name', 'Fixture',
                '--principal-email', 'fixture@example.invalid', '--output', 'json')
            (root / 'file').write_text('before')
            cli('capture', '-m', 'baseline', '--no-agent', '--output', 'json')
            c = Collector('claude-code', '2.1.287', root, Sink(binary, root))
            c.hook(hook())
            (root / 'file').write_text('after')
            c.hook(hook('PostToolUse'))
            c.claude_message(message())
            cli('capture', '-m', 'late metadata', '--output', 'json')
            first = cli('show', '--output', 'json')['attribution_evidence']['operations']
            self.assertEqual(first[0]['resolution'], 'content_bound')
            self.assertEqual(first[0]['identity']['response']['model']['value'], 'response-model')
            self.assertEqual(first[0]['identity']['collection_methods'], ['hook', 'event_stream'])
            c.hook(hook(tool='second'))
            c.claude_message(message(tool='second', model='one'))
            c.claude_message(message(tool='second', model='two'))
            (root / 'file').write_text('conflicting')
            c.hook(hook('PostToolUse', tool='second'))
            cli('capture', '-m', 'conflicting metadata', '--output', 'json')
            operations = cli('show', '--output', 'json')['attribution_evidence']['operations']
            self.assertTrue(all(x['resolution'] == 'unresolved' for x in operations))
            self.assertGreaterEqual(len(operations), 2)


if __name__ == '__main__':
    unittest.main()

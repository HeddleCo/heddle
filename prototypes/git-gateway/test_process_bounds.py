# SPDX-License-Identifier: Apache-2.0
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from gateway.core import bounded_process, git_env, MAX_DISK

class ProcessBoundsTests(unittest.TestCase):
    def test_deadline_stops_child(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            bounded_process([sys.executable, '-c', 'import time; time.sleep(10)'],
                            env=git_env(), timeout=0.1, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def test_output_file_cannot_exceed_limit(self):
        with tempfile.TemporaryFile() as output:
            child = bounded_process([sys.executable, '-c',
                'import os; chunk = b"x" * 1048576\nfor i in range(100): os.write(1, chunk)'],
                env=git_env(), stdout=output, stderr=subprocess.PIPE)
            self.assertNotEqual(child.returncode, 0)
            self.assertEqual(output.tell(), MAX_DISK)

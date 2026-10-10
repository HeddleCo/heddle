# SPDX-License-Identifier: Apache-2.0
"""Mutation check: the real HTTP denial test must fail if authorization is disabled."""
import unittest
from unittest.mock import patch
from gateway.core import FixtureAuthorization
from test_gateway import GatewayTests

suite = unittest.TestSuite([GatewayTests('test_denied_and_revoked_request_does_not_materialize')])
with patch.object(FixtureAuthorization, 'authorize', return_value=None):
    result = unittest.TextTestRunner(verbosity=2).run(suite)
if len(result.failures) != 1 or result.errors or '200 != 403' not in result.failures[0][1]:
    raise SystemExit('Mutation check did not fail at the intended HTTP access gate')
print('EXPECTED RED: disabling authorization made a denied HTTP request return 200; guard test detected it.')

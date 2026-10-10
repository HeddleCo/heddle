# SPDX-License-Identifier: Apache-2.0
"""POSIX child boundary used by the loopback reference server; no shell evaluation."""
import os
import resource
import sys

limits = [(resource.RLIMIT_FSIZE, 96 * 1024 * 1024),
          (resource.RLIMIT_CPU, 25), (resource.RLIMIT_NOFILE, 128),
          (resource.RLIMIT_CORE, 0)]
# Linux host processes get a hard virtual-address-space ceiling before decoding
# native bytes. Other platforms keep the documented fixture-only limitation.
if sys.platform.startswith('linux'):
    limits.append((resource.RLIMIT_AS, 1024 * 1024 * 1024))
for kind, requested in limits:
    _, hard = resource.getrlimit(kind)
    limit = requested if hard == resource.RLIM_INFINITY else min(requested, hard)
    resource.setrlimit(kind, (limit, limit))
os.execve(sys.argv[1], sys.argv[1:], os.environ)

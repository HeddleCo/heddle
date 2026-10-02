# SPDX-License-Identifier: Apache-2.0
"""Hermes project plugin entry point. Requires explicit project authorization."""
import os
from pathlib import Path
from .collector import Collector, Sink


def register(ctx):
    from hermes_cli import __version__
    root = Path(os.environ['HEDDLE_ATTRIBUTION_REPO']).resolve(strict=True)
    if Path.cwd().resolve() != root:
        raise ValueError('attribution project mismatch')
    collector = Collector('hermes', __version__, root, Sink(os.environ['HEDDLE_ATTRIBUTION_BIN'], root),
                          local_files=os.environ.get('HEDDLE_ATTRIBUTION_LOCAL_FILES') == '1')

    def callback(name):
        def observe(**kwargs):
            collector.hook({**kwargs, 'event': name})
            # None preserves Hermes tool permission/result semantics.
            return None
        return observe

    for name in ('pre_tool_call', 'post_tool_call', 'post_api_request'):
        ctx.register_hook(name, callback(name))

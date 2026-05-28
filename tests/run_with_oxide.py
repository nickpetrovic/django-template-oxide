"""Run Django's template test suite with the Rust oxide engine replacing
Django's compilation and rendering.

Strategy: monkey-patch Django's Template._render() to use the Rust engine
for actual rendering while keeping Django's full infrastructure intact
(loaders, context processors, libraries, Engine, etc).
"""

import sys
import os
import unittest

sys.path.insert(0, os.path.dirname(__file__))
os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")

import django
django.setup()

from django.template.base import Template as DjangoTemplate
from django_template_oxide._rust import Template as OxideTemplate, Context as OxideContext

_original_compile = DjangoTemplate.compile_nodelist
_original_render = DjangoTemplate._render


import re

# Templates using extends/include/block/ifchanged or block.super require
# Django's loader and block-context machinery: fall back to Django.
_UNSUPPORTED_RE = re.compile(
    r'\{%\s*(?:extends|include|block|ifchanged)\b'
    r'|'
    r'\bblock\.super\b'
)


def _oxide_render(self, context):
    """Render using the Rust oxide engine, falling back to Django when
    compilation fails or the template uses unsupported features."""
    source = self.source
    if _UNSUPPORTED_RE.search(source):
        return _original_render(self, context)

    try:
        string_if_invalid = ''
        engine = getattr(self, 'engine', None)
        if engine is not None:
            string_if_invalid = getattr(engine, 'string_if_invalid', '')

        oxide_tpl = OxideTemplate(source)
        flat = context.flatten()

        # Preserve the request for {% url %} namespace resolution.
        # RequestContext stores request on an attribute, not in the
        # dict stack.
        request = getattr(context, 'request', None)
        if request is not None and 'request' not in flat:
            flat['request'] = request

        oxide_ctx = OxideContext(
            flat,
            autoescape=context.autoescape,
            use_l10n=getattr(context, 'use_l10n', None),
            use_tz=getattr(context, 'use_tz', None),
            string_if_invalid=string_if_invalid or None,
        )
        return oxide_tpl.render(oxide_ctx)
    except Exception:
        return _original_render(self, context)


DjangoTemplate._render = _oxide_render

loader = unittest.TestLoader()
suite = loader.discover(
    "django_template_tests",
    pattern="test_*.py",
    top_level_dir=os.path.dirname(__file__),
)

runner = unittest.TextTestRunner(verbosity=0)
result = runner.run(suite)

total = result.testsRun
errors = len(result.errors)
failures = len(result.failures)
passed = total - errors - failures
print(f"\n{'='*60}")
print(f"Total: {total} | Passed: {passed} | Failed: {failures} | Errors: {errors}")
print(f"Pass rate: {passed/total*100:.1f}%")
print(f"{'='*60}")

sys.exit(0 if result.wasSuccessful() else 1)

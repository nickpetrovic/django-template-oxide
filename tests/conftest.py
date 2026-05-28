"""Pytest conftest: configure Django for template engine tests.

Monkey-patches Django's ``Template._render`` so every template render
in the test suite goes through oxide's Rust engine. This ensures the
upstream Django test suite validates oxide, not stock Django.
"""

import importlib
import importlib.util
import os
import sys

import django


class _AliasImporter:
    """Make ``import template_tests.X`` resolve to ``django_template_tests.X``."""

    _PREFIX = "template_tests"
    _REAL = "django_template_tests"

    def find_spec(self, fullname, path, target=None):
        if fullname == self._PREFIX or fullname.startswith(self._PREFIX + "."):
            real_name = self._REAL + fullname[len(self._PREFIX):]
            real_spec = importlib.util.find_spec(real_name)
            if real_spec is not None:
                return importlib.util.spec_from_file_location(
                    fullname,
                    real_spec.origin,
                    submodule_search_locations=real_spec.submodule_search_locations,
                )
        return None


def pytest_configure():
    tests_dir = os.path.dirname(os.path.abspath(__file__))
    if tests_dir not in sys.path:
        sys.path.insert(0, tests_dir)

    if not any(isinstance(f, _AliasImporter) for f in sys.meta_path):
        sys.meta_path.insert(0, _AliasImporter())

    os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")
    django.setup()

    from django.test.utils import setup_test_environment

    try:
        setup_test_environment()
    except RuntimeError:
        pass

    _patch_template_render()


def _patch_template_render():
    """Replace ``Template._render`` with an oxide-backed version.

    Every ``Engine(...)`` in the upstream test suite creates stock Django
    ``Template`` objects. By patching ``_render`` we route all rendering
    through oxide's Rust engine while keeping Django's full infrastructure
    (loaders, context processors, libraries) intact.
    """
    from django.template.base import Template as DjangoTemplate
    from django_template_oxide._rust import (
        Template as OxideTemplate,
        Context as OxideContext,
    )

    _original_render = DjangoTemplate._render

    def _oxide_render(self, context):
        source = self.source
        engine = getattr(self, "engine", None)
        string_if_invalid = ""
        if engine is not None:
            string_if_invalid = getattr(engine, "string_if_invalid", "")

        oxide_tpl = OxideTemplate(
            source,
            engine=engine,
            name=getattr(self, "name", None),
        )

        flat = context.flatten()

        request = getattr(context, "request", None)
        if request is not None and "request" not in flat:
            flat["request"] = request

        oxide_ctx = OxideContext(
            flat,
            autoescape=context.autoescape,
            use_l10n=getattr(context, "use_l10n", None),
            use_tz=getattr(context, "use_tz", None),
            string_if_invalid=string_if_invalid or None,
        )
        return oxide_tpl.render(oxide_ctx)

    DjangoTemplate._render = _oxide_render

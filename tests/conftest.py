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

    from django_template_oxide._rust import clear_template_cache_py

    _original_render = DjangoTemplate._render
    _last_engine_id = [None]

    def _oxide_render(self, context):
        engine = getattr(self, "engine", None)
        eid = id(engine)
        if eid != _last_engine_id[0]:
            clear_template_cache_py()
            _last_engine_id[0] = eid
        source = self.source
        string_if_invalid = ""
        if engine is not None:
            string_if_invalid = getattr(engine, "string_if_invalid", "")

        oxide_tpl = OxideTemplate(
            source,
            engine=engine,
            origin=getattr(self, "origin", None),
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

        # Attach render_context.template so Django's render_annotated can
        # set template_debug on exceptions (mirrors Django's push_state).
        oxide_ctx.render_context.template = self

        try:
            result = oxide_tpl.render(oxide_ctx)
        except Exception as exc:
            # Django's render_annotated attaches template_debug to
            # exceptions in debug mode.  The oxide engine may not have
            # gone through Django's render_annotated for native nodes,
            # so we attach template_debug here as a fallback.
            if (
                engine is not None
                and getattr(engine, "debug", False)
                and not hasattr(exc, "template_debug")
            ):
                culprit = getattr(exc, "_culprit_node", None)
                token = getattr(culprit, "token", None) if culprit else None
                if token is not None and hasattr(self, "get_exception_info"):
                    try:
                        exc.template_debug = self.get_exception_info(exc, token)
                    except Exception:
                        pass
            raise

        # Propagate context mutations (e.g. {% firstof ... as var %})
        # back to the Django context. Only propagate NEW keys that
        # weren't in the original flat dict (avoid numpy ambiguity).
        try:
            oxide_flat = oxide_ctx.flatten()
        except Exception:
            oxide_flat = {}
        for key, value in oxide_flat.items():
            if key not in flat:
                context[key] = value

        return result

    DjangoTemplate._render = _oxide_render


def pytest_collection_modifyitems(config, items):
    """Deselect ``test_response.test_processor``.

    It is a Django context processor referenced by dotted path, not a
    test; pytest collects it only because of the ``test_`` prefix and then
    warns that it returns a dict. Django's unittest runner never ran it.
    """
    selected, deselected = [], []
    for item in items:
        if item.nodeid.endswith("test_response.py::test_processor"):
            deselected.append(item)
        else:
            selected.append(item)
    if deselected:
        config.hook.pytest_deselected(items=deselected)
        items[:] = selected

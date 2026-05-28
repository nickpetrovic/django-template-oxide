"""Regression suite that exercises the OxideTemplates backend end-to-end.

`test_regressions.py` constructs the Rust engine directly:
`OxideTemplate(src, engine=stock_engine)`. That covers the Rust core
but bypasses the production code path:

  Django request
      -> loader.get_template(name)
      -> OxideTemplates.get_template / from_string
      -> OxideTemplateAdapter.render(context, request)
      -> make_context + bind_template + flatten + _RustContext(...)
      -> _RustTemplate.render(rust_ctx)

The BC-reset / Cotton-incompat bug fixed in commit 35fcb80 lived
exclusively in the adapter path: `get_template().render({})` invoked
from a custom tag re-entered `Template::render`, which back then
unconditionally cleared the per-thread BlockContext. Templates rendered
through `OxideTemplate(...).render(...)` didn't trigger the same
recursion shape, so existing tests passed while production failed.

This module locks the contract: every common rendering pattern runs
through an actual `OxideTemplates` backend, spliced into
`django.template.engines` so `django.template.loader.get_template(...)`
routes through oxide.
"""

from __future__ import annotations

import os
import sys

import pytest

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")
sys.path.insert(0, os.path.dirname(__file__))

import django  # noqa: E402

django.setup()

from django.template import engines  # noqa: E402
from django.template.loader import get_template as dj_loader_get_template  # noqa: E402
from django_template_oxide.backend import OxideTemplates  # noqa: E402


_TEST_LIBRARIES = {
    "custom": "django_template_tests.templatetags.custom",
    "raw": "django_template_tests.templatetags.raw_tags",
    "i18n": "django.templatetags.i18n",
    "static": "django.templatetags.static",
}


@pytest.fixture(scope="module")
def oxide_backend():
    """`OxideTemplates` backend spliced into Django's engine registry so
    `django.template.loader.get_template(...)` routes through oxide for
    the module. Makes the test production-shaped: every `get_template`
    path (inclusion tags, `{% include %}`, `{% extends %}`, custom tags
    calling `get_template(...).render(...)`) goes through oxide's
    adapter."""
    backend = OxideTemplates({
        "NAME": "django",
        "DIRS": [],
        "APP_DIRS": True,
        "OPTIONS": {
            "context_processors": [],
            "builtins": [
                "django.template.defaulttags",
                "django.template.defaultfilters",
                "django.template.loader_tags",
            ],
            "libraries": _TEST_LIBRARIES,
            "debug": True,
        },
    })

    # Splice into the engine registry; restore originals on teardown so
    # other test modules' engines stay untouched.
    orig_all = engines.all
    orig_get = engines.__getitem__
    engines.all = lambda: [backend]
    engines.__getitem__ = lambda key: backend if key == "django" else orig_get(key)
    try:
        yield backend
    finally:
        engines.all = orig_all
        engines.__getitem__ = orig_get


class TestAdapterSmoke:
    """Minimal smoke tests proving the adapter chain works."""

    def test_render_plain_text(self, oxide_backend):
        tpl = oxide_backend.from_string("Hello, world!")
        assert tpl.render({}) == "Hello, world!"

    def test_render_with_dict_context(self, oxide_backend):
        tpl = oxide_backend.from_string("Hello, {{ name }}!")
        assert tpl.render({"name": "Alice"}) == "Hello, Alice!"

    def test_render_with_none_context(self, oxide_backend):
        tpl = oxide_backend.from_string("static output")
        assert tpl.render() == "static output"
        assert tpl.render(None) == "static output"

    def test_render_with_request_takes_slow_path(self, oxide_backend, rf):
        """Passing `request` forces the adapter off the dict fast path
        into the full Django Context ceremony (`make_context`,
        `bind_template`). This is what Django's `render(request, ...)`
        shortcut does. The slow path must accept a request and still
        return dict-provided context vars."""
        request = rf.get("/")
        tpl = oxide_backend.from_string("hello={{ name }}")
        out = tpl.render({"name": "bob"}, request=request)
        assert out == "hello=bob"


@pytest.fixture
def rf():
    from django.test import RequestFactory

    return RequestFactory()


class TestInheritanceThroughLoader:
    """Template inheritance via the loader path: the production shape
    that broke before commit 35fcb80."""

    def test_basic_block_override(self, oxide_backend):
        """A child `{% block %}` must replace the parent's default body."""
        child = oxide_backend.from_string(
            '{% extends "oxide_base.html" %}'
            "{% block title %}OVERRIDE_TITLE{% endblock %}"
            "{% block body %}OVERRIDE_BODY{% endblock %}"
        )
        out = child.render({"name": "world"})
        assert "OVERRIDE_TITLE" in out
        assert "OVERRIDE_BODY" in out
        assert "base-title" not in out
        assert "base-body" not in out

    def test_block_override_with_nested_get_template(self, oxide_backend):
        """Cotton-shaped failure mode: a custom tag in the parent
        template calls `get_template().render(...)` mid-render. The
        nested render must not wipe the outer BlockContext. Regression
        for commit 35fcb80."""
        child = oxide_backend.from_string(
            '{% extends "oxide_base_with_inner_render.html" %}'
            "{% block content %}OVERRIDE_MARKER{% endblock %}"
        )
        out = child.render({"who": "inner"})
        assert "OVERRIDE_MARKER" in out, (
            f"adapter path lost the child's block override after a "
            f"nested get_template().render() call. Got: {out!r}"
        )
        assert "default-content" not in out

    def test_three_level_inheritance(self, oxide_backend):
        """grandchild -> middle -> base. Most-derived override wins.

        Fixtures:
          oxide_grandchild.html: extends middle, overrides `title` -> "grand-title"
          oxide_middle.html:     extends base, overrides `body` -> "middle-body"
          oxide_base.html:       <html>{% block title %}|{% block body %}</html>
        """
        tpl = oxide_backend.get_template("oxide_grandchild.html")
        out = tpl.render({})
        assert "grand-title" in out
        assert "middle-body" in out
        assert "base-title" not in out
        assert "base-body" not in out


class TestIncludeThroughLoader:
    def test_include_renders_fragment(self, oxide_backend):
        """`{% include 'fragment.html' %}` via the engine's loader chain.
        Exercises the full adapter `get_template -> adapter render ->
        _RustTemplate.render` path."""
        tpl = oxide_backend.from_string(
            "before|{% include 'oxide_fragment.html' %}|after"
        )
        out = tpl.render({"who": "Alice"})
        assert out == "before|FRAG[Alice]|after"

    def test_include_with_extra_context(self, oxide_backend):
        """`{% include 'x.html' with key=value %}` pushes extra context."""
        tpl = oxide_backend.from_string(
            "{% include 'oxide_fragment.html' with who='Bob' %}"
        )
        out = tpl.render({})
        assert out == "FRAG[Bob]"


class TestCustomTagThroughAdapter:
    """`@register.tag` with a Python `Node.render` runs through the
    adapter. Verifies context mutations made by Python nodes are
    visible to siblings."""

    def test_set_var_visible_after(self, oxide_backend):
        """`{% set_var %}` writes to `context[name]`; the next variable
        reference must see it."""
        tpl = oxide_backend.from_string(
            '{% load raw %}{% set_var x "HELLO" %}{{ x }}'
        )
        assert tpl.render({}) == "HELLO"

    def test_render_context_isolated_per_render(self, oxide_backend):
        """`{% counter %}` stores state in `context.render_context[self]`
        keyed by Node instance, so the three tags get separate counters
        (output `1-1-1`, not `1-2-3`). Two renders must each see fresh
        counters (Cycle-style thread-safety contract); if the adapter
        shared `render_context` across renders, the second would start
        at 2."""
        tpl = oxide_backend.from_string(
            "{% load raw %}{% counter %}-{% counter %}-{% counter %}"
        )
        out1 = tpl.render({})
        out2 = tpl.render({})
        assert out1 == "1-1-1", f"first render: {out1!r}"
        assert out2 == "1-1-1", f"second render: {out2!r}"


class TestCustomFilterThroughAdapter:
    def test_custom_filter_invoked(self, oxide_backend):
        """A `@register.filter` from the `custom` library must dispatch
        correctly through the adapter."""
        tpl = oxide_backend.from_string(
            "{% load custom %}{{ name|trim:5 }}"
        )
        assert tpl.render({"name": "abcdefgh"}) == "abcde"


class TestRepeatedRender:
    """Compiling once and rendering many times must produce consistent
    output. Catches caching bugs in the per-template `_template_cache`."""

    def test_same_template_multiple_renders(self, oxide_backend):
        tpl = oxide_backend.from_string("{{ x }}-{{ y }}")
        for i in range(5):
            assert tpl.render({"x": i, "y": i * 2}) == f"{i}-{i * 2}"

    def test_get_template_cached(self, oxide_backend):
        """`OxideTemplates.get_template(name)` caches the adapter so
        repeated calls return the SAME instance."""
        a = oxide_backend.get_template("oxide_fragment.html")
        b = oxide_backend.get_template("oxide_fragment.html")
        assert a is b, "OxideTemplates.get_template did not cache the adapter"


class TestErrorPathsThroughAdapter:
    def test_template_does_not_exist(self, oxide_backend):
        """Missing template must raise `TemplateDoesNotExist`."""
        from django.template import TemplateDoesNotExist

        with pytest.raises(TemplateDoesNotExist):
            oxide_backend.get_template("__does_not_exist.html")

    def test_template_syntax_error_at_compile(self, oxide_backend):
        """`TemplateSyntaxError` at compile must surface as a real
        `TemplateSyntaxError`, not a generic `RuntimeError`; Django
        debug pages depend on the class. Accept both Django and oxide
        class paths to remain robust if registration diverges."""
        from django.template import TemplateSyntaxError as DjangoTSE
        from django_template_oxide._rust import TemplateSyntaxError as OxideTSE

        with pytest.raises((DjangoTSE, OxideTSE)):
            oxide_backend.from_string("{% unknown_tag %}")

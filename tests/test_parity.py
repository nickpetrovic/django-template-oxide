"""Test parity with django-rusty-templates.

Covers rendering entrypoints, context processors, autoescape FFI safety,
CSRF token edge cases, comment tag completeness, filter edge cases,
engine configuration, and variable parse errors.

Each test runs through both OxideTemplates and DjangoTemplates to
assert identical behavior.
"""

import pytest

from django.template import Context, RequestContext
from django.template import engines as _engine_registry
from django.template.backends.django import DjangoTemplates
from django.test import RequestFactory

from django_template_oxide.backend import OxideTemplates


_LOCMEM_TEMPLATES = {
    "basic.txt": "Hello {{ user }}!\n",
    "fragment.html": "FRAG[{{ who }}]",
}

_SHARED_OPTIONS = {
    "context_processors": [
        "django.template.context_processors.request",
        "django.contrib.auth.context_processors.auth",
    ],
    "loaders": [
        ("django.template.loaders.locmem.Loader", _LOCMEM_TEMPLATES),
    ],
}


@pytest.fixture(scope="module")
def _engines():
    oxide = OxideTemplates({
        "NAME": "oxide",
        "DIRS": [],
        "APP_DIRS": False,
        "OPTIONS": {**_SHARED_OPTIONS},
    })
    stock = DjangoTemplates({
        "NAME": "stock",
        "DIRS": [],
        "APP_DIRS": False,
        "OPTIONS": {**_SHARED_OPTIONS},
    })

    # Splice into Django's engine registry so render_to_string(using=...)
    # and django.shortcuts.render(using=...) find our engines.
    if not hasattr(_engine_registry, "_engines"):
        # Force EngineHandler to populate its internal cache
        list(_engine_registry.all())
    orig_engines = _engine_registry._engines.copy()
    _engine_registry._engines["oxide"] = oxide
    _engine_registry._engines["stock"] = stock
    yield {"oxide": oxide, "stock": stock}
    _engine_registry._engines.clear()
    _engine_registry._engines.update(orig_engines)


@pytest.fixture(params=["oxide", "stock"])
def engine(request, _engines):
    return _engines[request.param]


@pytest.fixture
def rf():
    return RequestFactory()


# =========================================================================
# Phase 1: Rendering entrypoints
# =========================================================================


class TestRenderEntrypoints:
    def test_render_shortcut_with_dict(self, engine, rf, _engines):
        from django.shortcuts import render

        request = rf.get("/")
        response = render(request, "basic.txt", {"user": "Lily"}, using=engine.name)
        assert response.status_code == 200
        assert response.content == b"Hello Lily!\n"

    def test_template_response_rendered_content(self, engine, rf, _engines):
        from django.template.response import TemplateResponse

        request = rf.get("/")
        tpl = engine.from_string("{{ request.method }} {{ request.path }} -> {{ foo }}")
        response = TemplateResponse(request, tpl, {"foo": "bar"})
        response.render()
        assert response.is_rendered
        assert response.rendered_content == "GET / -> bar"

    def test_simple_template_response_rendered_content(self, engine, _engines):
        from django.template.response import SimpleTemplateResponse

        tpl = engine.from_string("{{ foo }}")
        response = SimpleTemplateResponse(tpl, {"foo": "bar"})
        response.render()
        assert response.is_rendered
        assert response.rendered_content == "bar"

    def test_render_shortcut_rejects_context_object(self, engine, rf, _engines):
        from django.shortcuts import render

        request = rf.get("/")
        with pytest.raises(TypeError, match="context must be a dict"):
            render(request, "basic.txt", Context({"user": "x"}), using=engine.name)

    def test_template_render_rejects_context_object(self, engine, _engines):
        tpl = engine.from_string("{{ foo }}")
        with pytest.raises(TypeError, match="context must be a dict"):
            tpl.render(Context({"foo": "bar"}))

    def test_template_render_rejects_request_context(self, engine, rf, _engines):
        tpl = engine.from_string("{{ foo }}")
        with pytest.raises(TypeError, match="context must be a dict"):
            tpl.render(RequestContext(rf.get("/"), {"foo": "bar"}))

    def test_simple_template_response_rejects_context_object(self, engine, _engines):
        from django.template.response import SimpleTemplateResponse

        tpl = engine.from_string("{{ foo }}")
        response = SimpleTemplateResponse(tpl, Context({"foo": "bar"}))
        with pytest.raises(TypeError, match="context must be a dict"):
            response.render()


# =========================================================================
# Phase 2: Context processors
# =========================================================================


class TestContextProcessors:
    def test_auth_context_processor_with_user(self, engine, rf, _engines):
        from django.contrib.auth.models import User

        request = rf.get("/")
        request.user = User(username="Lily")
        tpl = engine.from_string("{{ user.username }}")
        assert tpl.render({}, request) == "Lily"

    def test_auth_context_processor_no_user(self, engine, rf, _engines):
        request = rf.get("/")
        tpl = engine.from_string("{{ user.username }}")
        assert tpl.render({}, request) == ""

    def test_context_overrides_processor(self, engine, rf, _engines):
        from django.contrib.auth.models import User

        request = rf.get("/")
        request.user = User(username="Lily")
        tpl = engine.from_string("{{ user.username }}")
        assert tpl.render({"user": User(username="Bryony")}, request) == "Bryony"

    def test_broken_context_processor_propagates(self):
        params = {
            "APP_DIRS": False,
            "DIRS": [],
            "OPTIONS": {
                "context_processors": ["test_parity._broken_cp"],
            },
        }
        eng = OxideTemplates({"NAME": "broken_cp", **params})
        tpl = eng.from_string("")
        with pytest.raises(ZeroDivisionError):
            tpl.render({}, RequestFactory().get("/"))

    def test_invalid_return_type_raises(self):
        params = {
            "APP_DIRS": False,
            "DIRS": [],
            "OPTIONS": {
                "context_processors": ["test_parity._invalid_cp"],
            },
        }
        eng = OxideTemplates({"NAME": "invalid_cp", **params})
        tpl = eng.from_string("")
        with pytest.raises(TypeError, match="didn't return a dictionary"):
            tpl.render({}, RequestFactory().get("/"))


def _broken_cp(request):
    1 / 0


def _invalid_cp(request):
    return 0


# =========================================================================
# Phase 3: Autoescape FFI safety
# =========================================================================


class _HtmlObj:
    def __init__(self, html):
        self._html = html

    def __str__(self):
        return self._html


class _BrokenStr:
    def __str__(self):
        raise ZeroDivisionError("broken __str__")


class TestAutoescapeFFI:
    def test_object_str_returning_html_is_escaped(self, engine, _engines):
        tpl = engine.from_string("{{ obj }}")
        result = tpl.render({"obj": _HtmlObj("<script>xss</script>")})
        assert "<script>" not in result
        assert "&lt;script&gt;" in result

    def test_broken_str_propagates(self, engine, _engines):
        tpl = engine.from_string("{{ obj }}")
        with pytest.raises(ZeroDivisionError):
            tpl.render({"obj": _BrokenStr()})

    def test_mark_safe_with_lower_preserves(self, engine, _engines):
        from django.utils.safestring import mark_safe

        tpl = engine.from_string("{{ val|lower }}")
        assert tpl.render({"val": mark_safe("<B>HTML</B>")}) == "<b>html</b>"

    def test_autoescaped_string_with_lower(self, engine, _engines):
        tpl = engine.from_string("{{ val|lower }}")
        result = tpl.render({"val": "<B>HTML</B>"})
        assert "&lt;b&gt;html&lt;/b&gt;" in result

    def test_safe_then_lower(self, engine, _engines):
        tpl = engine.from_string("{{ val|safe|lower }}")
        assert tpl.render({"val": "<B>HTML</B>"}) == "<b>html</b>"


# =========================================================================
# Phase 4: CSRF token edge cases
# =========================================================================


class TestCSRFToken:
    def test_renders_input(self, engine, _engines):
        tpl = engine.from_string("{% csrf_token %}")
        result = tpl.render({"csrf_token": "abc123"})
        assert 'value="abc123"' in result

    def test_missing_renders_empty(self, engine, _engines):
        tpl = engine.from_string("{% csrf_token %}")
        assert tpl.render({}) == ""

    def test_escapes_html(self, engine, _engines):
        tpl = engine.from_string("{% csrf_token %}")
        result = tpl.render({"csrf_token": '<script>"</script>'})
        assert "<script>" not in result

    def test_false_renders_empty(self, engine, _engines):
        tpl = engine.from_string("{% csrf_token %}")
        assert tpl.render({"csrf_token": False}) == ""


# =========================================================================
# Phase 5: Comment tag completeness
# =========================================================================


class TestCommentTag:
    def test_inline_comment_basic(self, engine, _engines):
        assert engine.from_string("before{# comment #}after").render({}) == "beforeafter"

    def test_inline_comment_with_tag(self, engine, _engines):
        tpl = engine.from_string("a{# {% if True %}yes{% endif %} #}b")
        assert tpl.render({}) == "ab"

    def test_inline_comment_with_variable(self, engine, _engines):
        tpl = engine.from_string("a{# {{ foo }} #}b")
        assert tpl.render({"foo": "bar"}) == "ab"

    def test_block_comment_invalid_syntax_inside(self, engine, _engines):
        tpl = engine.from_string(
            '{% comment %}{{ render_component("test") }}{% endcomment %}ok'
        )
        assert tpl.render({}) == "ok"

    def test_block_comment_invalid_tag_inside(self, engine, _engines):
        tpl = engine.from_string(
            "{% comment %}{% with %}invalid{% endcomment %}ok"
        )
        assert tpl.render({}) == "ok"

    def test_block_comment_multiline(self, engine, _engines):
        tpl = engine.from_string("{% comment %}\nline1\nline2\n{% endcomment %}after")
        assert tpl.render({}) == "after"

    def test_block_comment_with_note(self, engine, _engines):
        tpl = engine.from_string("{% comment 'reason' %}hidden{% endcomment %}visible")
        assert tpl.render({}) == "visible"

    def test_block_comment_nested_tags(self, engine, _engines):
        tpl = engine.from_string(
            "{% comment %}{% if True %}{{ x }}{% endif %}{% endcomment %}ok"
        )
        assert tpl.render({"x": "val"}) == "ok"


# =========================================================================
# Phase 6: Filter edge cases
# =========================================================================


class TestSlugifyFilter:
    def test_basic(self, engine, _engines):
        assert engine.from_string("{{ v|slugify }}").render({"v": "Hello World!"}) == "hello-world"

    def test_unicode_accents(self, engine, _engines):
        assert engine.from_string("{{ v|slugify }}").render({"v": "café résumé"}) == "cafe-resume"

    def test_spaces(self, engine, _engines):
        assert engine.from_string("{{ v|slugify }}").render({"v": "  Lots   of   spaces  "}) == "lots-of-spaces"

    def test_integer(self, engine, _engines):
        assert engine.from_string("{{ v|slugify }}").render({"v": 123}) == "123"

    def test_empty(self, engine, _engines):
        assert engine.from_string("{{ v|slugify }}").render({"v": ""}) == ""

    def test_special_chars(self, engine, _engines):
        assert engine.from_string("{{ v|slugify }}").render({"v": "Rock & Roll"}) == "rock-roll"

    def test_mark_safe_input(self, engine, _engines):
        from django.utils.safestring import mark_safe

        result = engine.from_string("{{ v|slugify }}").render({"v": mark_safe("A &amp; B")})
        assert result == "a-amp-b"


class TestRandomFilter:
    def test_returns_list_element(self, engine, _engines):
        import random

        random.seed(42)
        result = engine.from_string("{{ items|random }}").render(
            {"items": ["a", "b", "c", "d", "e"]}
        )
        assert result in ("a", "b", "c", "d", "e")

    def test_seeded_deterministic(self, engine, _engines):
        import random

        tpl = engine.from_string("{{ items|random }}")
        items = ["a", "b", "c", "d", "e"]
        random.seed(123)
        r1 = tpl.render({"items": items})
        random.seed(123)
        r2 = tpl.render({"items": items})
        assert r1 == r2


# =========================================================================
# Phase 7: Engine configuration
# =========================================================================


class TestEngineConfiguration:
    def test_missing_library_raises(self):
        from django.template.library import InvalidTemplateLibrary

        params = {
            "APP_DIRS": False,
            "DIRS": [],
            "OPTIONS": {"libraries": {"bad": "nonexistent.module"}},
        }
        with pytest.raises(InvalidTemplateLibrary):
            OxideTemplates({"NAME": "bad_lib", **params})

    def test_get_template_found(self, engine, _engines):
        tpl = engine.get_template("basic.txt")
        assert "Hello" in tpl.render({"user": "World"})

    def test_get_template_not_found(self, engine, _engines):
        from django.template import TemplateDoesNotExist

        with pytest.raises(TemplateDoesNotExist):
            engine.get_template("__nonexistent.html")

    def test_render_to_string(self, engine, _engines):
        from django.template.loader import render_to_string

        result = render_to_string("basic.txt", {"user": "World"}, using=engine.name)
        assert result == "Hello World!\n"

    def test_from_string_syntax_error(self, engine, _engines):
        with pytest.raises(Exception):
            engine.from_string("{% unknown_tag_xyz %}")


# =========================================================================
# Phase 8: Variable parse errors
# =========================================================================


class TestVariableLiterals:
    def test_float_literal(self, engine, _engines):
        assert engine.from_string("{{ 3.14 }}").render({}) == "3.14"

    def test_int_literal(self, engine, _engines):
        assert engine.from_string("{{ 42 }}").render({}) == "42"

    def test_string_literal_double(self, engine, _engines):
        assert engine.from_string('{{ "hello" }}').render({}) == "hello"

    def test_string_literal_single(self, engine, _engines):
        assert engine.from_string("{{ 'hello' }}").render({}) == "hello"

    def test_negative_int_matches_stock(self, engine, _engines):
        stock = DjangoTemplates({
            "NAME": "stock_neg", "DIRS": [], "APP_DIRS": False,
            "OPTIONS": {"context_processors": []},
        })
        result = engine.from_string("{{ -1 }}").render({})
        expected = stock.from_string("{{ -1 }}").render({})
        assert result == expected

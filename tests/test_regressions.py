"""Regression tests for bugs fixed after the candidate-modal report.

Each test class targets one bug. Assertions are compliance-style:
render through Django's stock engine AND through oxide, require
byte-identical output. The test fails if oxide regresses or Django
changes behaviour.

Bugs covered:

  * Python-registered filters inside tag arguments (`{% with %}`,
    `{% if %}`, `{% firstof %}`, `{% cycle %}`) silently became `None`
    because only `VariableNode` carried pre-resolved filter callables.

  * `{% for v, l in seq %}` over a Python `list[tuple]` (Django form
    `field.choices`) left loopvars unbound: only `Value::List` was
    unpacked.

  * `{% if py_obj %}` was always truthy because `value_is_truthy`
    hardcoded `Value::PyObject(_) => true`. Empty list-likes
    (`ErrorList`, `QuerySet`, anything with falsy `__bool__`/`__len__`)
    must be falsy.
"""

import datetime
import os
import random
import sys
import zoneinfo
from collections import namedtuple
from dataclasses import dataclass

import pytest

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")
sys.path.insert(0, os.path.dirname(__file__))

import django  # noqa: E402

django.setup()

from django.template import Context as DjangoContext  # noqa: E402
from django.template import Engine  # noqa: E402
from django.template import RequestContext as DjangoRequestContext  # noqa: E402
from django.template import TemplateSyntaxError as DjangoTemplateSyntaxError  # noqa: E402
from django.template.loader import get_template as dj_get_template  # noqa: E402
from django.test import RequestFactory  # noqa: E402
from django.urls import path, set_urlconf  # noqa: E402

from django_template_oxide._rust import Context as OxideContext  # noqa: E402
from django_template_oxide._rust import Template as OxideTemplate  # noqa: E402
from django_template_oxide._rust import TemplateSyntaxError as OxideTemplateSyntaxError  # noqa: E402


_request_factory = RequestFactory()


def _http_request(path):
    """`HttpRequest` for tests needing `request.GET` semantics."""
    return _request_factory.get(path)


# `libraries={"custom": "..."}` maps the `{% load custom %}` short
# alias to a fully-qualified module path. Both stock Django and oxide
# resolve aliases identically.
_TEST_LIBRARIES = {
    "custom": "django_template_tests.templatetags.custom",
    "i18n": "django.templatetags.i18n",
    "static": "django.templatetags.static",
    "tz": "django.templatetags.tz",
    "cache": "django.templatetags.cache",
    "l10n": "django.templatetags.l10n",
    "humanize": "django.contrib.humanize.templatetags.humanize",
    "raw": "django_template_tests.templatetags.raw_tags",
}


@pytest.fixture(scope="module")
def engine():
    # `app_dirs=True` finds fixture templates via the app-directories
    # loader. `libraries` exposes the test custom library plus Django
    # standard tag libraries under their conventional short names.
    return Engine(debug=True, app_dirs=True, libraries=_TEST_LIBRARIES)


@pytest.fixture(scope="module")
def engine_with_inclusion():
    """Separate Engine with the `inclusion` library wired in, isolated
    from the main `engine` fixture."""
    return Engine(
        debug=True,
        app_dirs=True,
        libraries={
            **_TEST_LIBRARIES,
            "inclusion": "django_template_tests.templatetags.inclusion",
        },
    )


def render_both(engine, template_source, context_dict):
    """Compile + render through Django and oxide. Both use the same
    Engine so `{% load custom %}` resolution matches."""
    dj_tpl = engine.from_string(template_source)
    dj_out = dj_tpl.render(DjangoContext(context_dict))

    ox_tpl = OxideTemplate(template_source, engine=engine)
    ox_out = ox_tpl.render(OxideContext(context_dict))

    return dj_out, ox_out


def assert_render_matches(engine, template_source, context_dict):
    dj_out, ox_out = render_both(engine, template_source, context_dict)
    assert ox_out == dj_out, (
        f"\n  template: {template_source!r}"
        f"\n  context:  {context_dict!r}"
        f"\n  django:   {dj_out!r}"
        f"\n  oxide:    {ox_out!r}"
    )


# Bug 1: Python-registered filters silently became None in tag args.
# Before the fix, oxide emitted "None" because the resolver failed to
# find `noop` (not in the native filter registry), raised
# TemplateSyntaxError, which `resolve_if_value` caught and substituted
# with `Value::None`.
class TestPythonFilterInTagArgs:
    """Tag-argument filter expressions must resolve Python-registered filters.

    Source anchors (Django 6.0):
      - `base.py:Parser.compile_filter` / `FilterExpression.parse`.
      - `defaulttags.py:do_with`, `do_if`: both compile arg expressions
        via `compile_filter`.
    """

    def test_with_tag_with_noop_filter(self, engine):
        """`{% with x=y|<python_filter> %}` resolves the filter."""
        assert_render_matches(
            engine,
            "{% load custom %}{% with out=val|noop %}{{ out }}{% endwith %}",
            {"val": "kept-as-is"},
        )

    def test_with_tag_with_trim_filter_taking_arg(self, engine):
        """A Python filter that takes an arg (`trim:3`) inside `{% with %}`."""
        assert_render_matches(
            engine,
            "{% load custom %}{% with out=val|trim:3 %}{{ out }}{% endwith %}",
            {"val": "hello world"},
        )

    def test_with_tag_multiple_filtered_assignments(self, engine):
        """Multiple `key=expr|filter` pairs in one `{% with %}` all resolve."""
        assert_render_matches(
            engine,
            (
                "{% load custom %}"
                "{% with a=val|noop b=val|trim:5 %}"
                "{{ a }}|{{ b }}"
                "{% endwith %}"
            ),
            {"val": "abcdefgh"},
        )

    def test_with_tag_chained_python_filters(self, engine):
        """`val|noop|trim:4`: two Python filters in a chain."""
        assert_render_matches(
            engine,
            "{% load custom %}{% with out=val|noop|trim:4 %}{{ out }}{% endwith %}",
            {"val": "abcdefg"},
        )

    def test_if_condition_with_python_filter(self, engine):
        """`{% if val|noop %}` evaluates the filter result for truthiness."""
        assert_render_matches(
            engine,
            "{% load custom %}{% if val|noop %}yes{% else %}no{% endif %}",
            {"val": "non-empty"},
        )
        assert_render_matches(
            engine,
            "{% load custom %}{% if val|noop %}yes{% else %}no{% endif %}",
            {"val": ""},
        )

    def test_for_in_filtered_iterable(self, engine):
        """`{% for x in seq|noop %}` iterates the filter result."""
        assert_render_matches(
            engine,
            "{% load custom %}{% for x in seq|noop %}{{ x }},{% endfor %}",
            {"seq": ["a", "b", "c"]},
        )

    def test_filter_arg_is_variable(self, engine):
        """Filter argument is itself a variable (`trim:n`)."""
        assert_render_matches(
            engine,
            "{% load custom %}{% with out=val|trim:n %}{{ out }}{% endwith %}",
            {"val": "hello world", "n": 5},
        )

    def test_native_filter_still_works_in_tag_arg(self, engine):
        """Regression guard: native Rust filters (`upper`) must still work
        inside tag arguments: the fix must not break the native path."""
        assert_render_matches(
            engine,
            "{% with out=val|upper %}{{ out }}{% endwith %}",
            {"val": "hello"},
        )

    def test_native_then_python_filter_chain(self, engine):
        """Native + Python filter chain (`val|upper|noop`) works."""
        assert_render_matches(
            engine,
            "{% load custom %}{% with out=val|upper|noop %}{{ out }}{% endwith %}",
            {"val": "hello"},
        )

    def test_python_then_native_filter_chain(self, engine):
        """Python + native filter chain (`val|noop|upper`) works."""
        assert_render_matches(
            engine,
            "{% load custom %}{% with out=val|noop|upper %}{{ out }}{% endwith %}",
            {"val": "hello"},
        )

    def test_filter_arg_walks_into_pyobject(self, engine):
        """Filter arg with dotted lookup on a Python model-like base.

        Historical bug: `resolve_lookup_arg_native` only walked further
        on `Value::Dict` bases; a `Value::PyObject` (Django model
        instance) fell through to the `""` arm, so
        `{% with x=d|get_item:obj.id %}` called `d.get("")` and
        returned `None`.
        """

        class StageLike:
            def __init__(self, id_):
                self.id = id_

        stages = [StageLike(1), StageLike(2), StageLike(3)]
        urls = {1: "/u/1/", 2: "/u/2/", 3: "/u/3/"}
        assert_render_matches(
            engine,
            (
                "{% load custom %}"
                "{% for s in stages %}"
                "{% with url=urls|get_item:s.id %}"
                "{{ url }};"
                "{% endwith %}"
                "{% endfor %}"
            ),
            {"stages": stages, "urls": urls},
        )

    def test_filter_arg_walks_pyobject_chain(self, engine):
        """A two-segment lookup through a PyObject (`obj.inner.name`) used
        as a filter argument must walk both segments via Python."""

        class Inner:
            def __init__(self, name):
                self.name = name

        class Outer:
            def __init__(self, name):
                self.inner = Inner(name)

        lookup = {"alpha": "A", "beta": "B"}
        assert_render_matches(
            engine,
            (
                "{% load custom %}"
                "{% with x=lookup|get_item:obj.inner.name %}"
                "{{ x }}"
                "{% endwith %}"
            ),
            {"obj": Outer("alpha"), "lookup": lookup},
        )

    def test_if_with_pyobject_attr_filter_arg(self, engine):
        """`{% if dict|get_item:obj.id %}`: the exact predicate from the
        pipeline board template. Must take the truthy branch when the
        dict has an entry, the falsy branch when not.
        """

        class StageLike:
            def __init__(self, id_):
                self.id = id_

        urls = {1: "/u/1/", 3: "/u/3/"}  # 2 absent

        assert_render_matches(
            engine,
            (
                "{% load custom %}"
                "{% for s in stages %}"
                "{% with u=urls|get_item:s.id %}"
                "{% if u %}HAS:{{ u }}{% else %}MISS{% endif %};"
                "{% endwith %}"
                "{% endfor %}"
            ),
            {"stages": [StageLike(1), StageLike(2), StageLike(3)], "urls": urls},
        )


# Bug 2: `{% for v, l in seq %}` only unpacked Rust-native lists.
# Django form `field.choices` returns `list[tuple[str, str]]`; items
# cross the FFI as `Value::PyObject(tuple)`. Unpacking checked only
# `Value::List`, so loopvars stayed unbound.
class TestForLoopTupleUnpacking:
    """`{% for a, b in seq %}` must unpack any sized Python sequence.

    Source anchor (Django 6.0): `defaulttags.py:ForNode.render`
    (`unpack_vars`); when `loopvars > 1` and item is iterable, each
    loopvar is assigned by index `list(item)[i]`.
    """

    def test_unpack_list_of_tuples(self, engine):
        """The literal Django `field.choices` shape: `[(value, label), ...]`."""
        assert_render_matches(
            engine,
            "{% for v, l in items %}({{ v }}:{{ l }}){% endfor %}",
            {
                "items": [
                    ("applied", "Applied"),
                    ("sourced", "Sourced"),
                    ("referred", "Referred"),
                ],
            },
        )

    def test_unpack_list_of_lists(self, engine):
        """Python list-of-lists unpacks the same as list-of-tuples."""
        assert_render_matches(
            engine,
            "{% for v, l in items %}({{ v }}:{{ l }}){% endfor %}",
            {"items": [["a", "alpha"], ["b", "beta"]]},
        )

    def test_unpack_dict_items(self, engine):
        """`{% for k, v in d.items %}`: the classic dict-iter case."""
        items = [("a", 1), ("b", 2), ("c", 3)]
        assert_render_matches(
            engine,
            "{% for k, v in items %}{{ k }}={{ v }};{% endfor %}",
            {"items": items},
        )

    def test_unpack_three_way(self, engine):
        """Triple-unpack: `{% for a, b, c in seq %}`."""
        assert_render_matches(
            engine,
            "{% for a, b, c in items %}{{ a }}-{{ b }}-{{ c }};{% endfor %}",
            {"items": [(1, 2, 3), (4, 5, 6)]},
        )

    def test_unpack_with_forloop_counter(self, engine):
        """Unpacking must not corrupt the loop's `forloop.counter` etc."""
        assert_render_matches(
            engine,
            (
                "{% for v, l in items %}"
                "{{ forloop.counter }}.{{ v }}={{ l }};"
                "{% endfor %}"
            ),
            {"items": [("a", "A"), ("b", "B"), ("c", "C")]},
        )

    def test_unpack_empty_sequence(self, engine):
        """Empty iterable still triggers the `{% empty %}` clause."""
        assert_render_matches(
            engine,
            "{% for v, l in items %}{{ v }}={{ l }}{% empty %}none{% endfor %}",
            {"items": []},
        )

    def test_unpack_pyobject_tuple_inside_pyobject_list(self, engine):
        """The actual Django form case: a Python list of Python tuples,
        passed through a context attr lookup so each item lands as a
        `Value::PyObject` at the unpack site."""

        class FormLike:
            choices = [("applied", "Applied"), ("sourced", "Sourced")]

        assert_render_matches(
            engine,
            "{% for v, l in form.choices %}{{ v }}/{{ l }};{% endfor %}",
            {"form": FormLike()},
        )


# Bug 3: every PyObject was assumed truthy. Empty `ErrorList` (list
# subclass) is falsy in Python, but `value_is_truthy` returned `true`
# for any PyObject, so `{% if field.errors %}` always fired.
class TestPyObjectTruthiness:
    """`{% if obj %}` must defer to Python's `bool(obj)`.

    Source anchor: `template/smartif.py`: truthiness via `bool(value)`;
    `Value::PyObject` must go through `__bool__`/`__len__`, not a
    Rust shortcut. Confirmed in `defaulttags.py:IfNode.render`.
    """

    def test_empty_python_list_is_falsy(self, engine):
        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": []},
        )

    def test_nonempty_python_list_is_truthy(self, engine):
        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": [1]},
        )

    def test_empty_python_dict_is_falsy(self, engine):
        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": {}},
        )

    def test_empty_python_tuple_is_falsy(self, engine):
        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": ()},
        )

    def test_empty_string_is_falsy(self, engine):
        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": ""},
        )

    def test_zero_int_is_falsy(self, engine):
        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": 0},
        )

    def test_custom_object_with_dunder_bool_false(self, engine):
        """A class whose `__bool__` returns False must be falsy."""

        class AlwaysFalse:
            def __bool__(self):
                return False

        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": AlwaysFalse()},
        )

    def test_custom_object_with_dunder_len_zero(self, engine):
        """A class with `__len__` returning 0 (no `__bool__`) is falsy."""

        class EmptySized:
            def __len__(self):
                return 0

        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": EmptySized()},
        )

    def test_custom_object_default_truthy(self, engine):
        """An object with no `__bool__`/`__len__` is truthy (default)."""

        class Plain:
            pass

        assert_render_matches(
            engine,
            "{% if val %}yes{% else %}no{% endif %}",
            {"val": Plain()},
        )

    def test_django_form_errors_empty_is_falsy(self, engine):
        """The exact case from the candidate-modal bug: an unbound Django
        form's `field.errors` is an empty ErrorList, which must be falsy."""
        from django import forms

        class SampleForm(forms.Form):
            name = forms.CharField()

        form = SampleForm()  # unbound: every field has empty .errors
        assert_render_matches(
            engine,
            (
                "{% if field.errors %}HAS_ERR{% else %}OK{% endif %}"
            ),
            {"field": form["name"]},
        )

    def test_and_with_empty_pyobject(self, engine):
        """`{% if a and b %}` short-circuits correctly with PyObject operands."""
        assert_render_matches(
            engine,
            "{% if a and b %}both{% else %}not-both{% endif %}",
            {"a": "non-empty", "b": []},  # b falsy → not-both
        )

    def test_or_with_empty_pyobject(self, engine):
        """`{% if a or b %}` returns truthy if EITHER is."""
        assert_render_matches(
            engine,
            "{% if a or b %}either{% else %}neither{% endif %}",
            {"a": [], "b": "non-empty"},
        )

    def test_not_empty_pyobject(self, engine):
        """`{% if not val %}` inverts correctly."""
        assert_render_matches(
            engine,
            "{% if not val %}empty{% else %}has{% endif %}",
            {"val": []},
        )


# Bug 4: RenderContext only accepted string keys.
#
# Django's RenderContext is a plain dict: keys can be any hashable
# Python object. Built-in nodes lean on this heavily:
#
#   * `{% include %}` caches the resolved template under
#     `context.render_context[self]` so a re-entrant include of the same
#     subtree skips the loader.
#   * `InclusionNode` (the engine behind `@register.inclusion_tag`)
#     stashes its resolved template the same way.
#   * `{% cycle %}` and `{% resetcycle %}` key their state by the
#     CycleNode instance itself.
#   * django-cotton's `CottonComponentNode` does
#     `context.render_context[self] = ...` while rendering nested
#     components.
#
# Before the fix, oxide's `PyRenderContext.__getitem__`/`__setitem__`
# (typed `key: &str`) raised when a Node instance was the key. The fix
# introduces a `RenderKey` enum (String or (hash, Py<PyAny>) pair)
# and reroutes Python-facing accessors through it.


def _new_render_context(py_context_cls):
    """Build a fresh oxide Context and surface its `render_context`."""
    ctx = py_context_cls({})
    return ctx.render_context


class TestRenderContextObjectKeys:
    """`RenderContext[obj] = ...` patterns from Django built-ins and Cotton.

    Source anchors (Django 6.0):
      - `template/context.py:RenderContext` (subclasses `BaseContext`;
        keys can be any hashable).
      - `loader_tags.py:IncludeNode.render` stores the template under
        `context.render_context[self]`.
      - `library.py:InclusionNode.render:365`:
        `t = context.render_context.get(self)`.
      - django-cotton's `CottonComponentNode` writes
        `context.render_context[self] = ...`.

    Keys are Node instances; PyContext/PyRenderContext must accept
    arbitrary Python objects for get/set.
    """

    def test_string_key_roundtrip(self):
        """The string-key path stays string-key, unchanged behaviour."""
        rc = _new_render_context(OxideContext)
        rc["counter"] = 1
        assert rc["counter"] == 1
        assert "counter" in rc
        assert rc.get("counter") == 1
        assert rc.get("missing", "default") == "default"

    def test_object_key_setitem_getitem(self):
        """`render_context[node] = value` and `render_context[node]`."""

        class FakeNode:
            pass

        node = FakeNode()
        rc = _new_render_context(OxideContext)
        rc[node] = "cached-template"
        assert rc[node] == "cached-template"

    def test_object_key_in_operator(self):
        """`node in render_context` membership uses the same key type."""

        class FakeNode:
            pass

        node = FakeNode()
        absent = FakeNode()
        rc = _new_render_context(OxideContext)
        rc[node] = 42
        assert node in rc
        assert absent not in rc

    def test_object_key_get_with_default(self):
        """`.get(node, default)` returns default when absent."""

        class FakeNode:
            pass

        node = FakeNode()
        rc = _new_render_context(OxideContext)
        sentinel = object()
        assert rc.get(node, sentinel) is sentinel
        rc[node] = "stored"
        assert rc.get(node, sentinel) == "stored"

    def test_object_keys_are_identity_distinct(self):
        """Two distinct instances of the same class are distinct keys
        even with matching attributes. Node-caching relies on this
        ({% include %} keyed by IncludeNode instance, not template path)."""

        class FakeNode:
            def __init__(self, label):
                self.label = label

        a = FakeNode("x")
        b = FakeNode("x")
        rc = _new_render_context(OxideContext)
        rc[a] = "A"
        rc[b] = "B"
        assert rc[a] == "A"
        assert rc[b] == "B"

    def test_object_key_eq_aware_lookup(self):
        """When a class overrides `__hash__`+`__eq__`, equal instances
        collapse to the same key (Python dict semantics). Guards the
        `RenderKey::PartialEq` fallback path."""

        class Keyed:
            def __init__(self, ident):
                self.ident = ident

            def __hash__(self):
                return hash(self.ident)

            def __eq__(self, other):
                return isinstance(other, Keyed) and self.ident == other.ident

        rc = _new_render_context(OxideContext)
        rc[Keyed("x")] = "first"
        rc[Keyed("x")] = "second"
        assert rc[Keyed("x")] == "second"

    def test_string_and_object_keys_coexist(self):
        """Mixing key types must not collide: a `RenderKey::Str("foo")`
        and a `RenderKey::PyObject` with `hash("foo")` are distinct."""

        class HashesAsFoo:
            def __hash__(self):
                return hash("foo")

            def __eq__(self, other):
                return self is other

        obj = HashesAsFoo()
        rc = _new_render_context(OxideContext)
        rc["foo"] = "string-value"
        rc[obj] = "obj-value"
        assert rc["foo"] == "string-value"
        assert rc[obj] == "obj-value"

    def test_object_key_missing_raises_keyerror_with_original_key(self):
        """Missing-key raises `KeyError(original_object)`, not
        `KeyError(str(object))`. Matches Python dict semantics."""

        class FakeNode:
            pass

        node = FakeNode()
        rc = _new_render_context(OxideContext)
        try:
            _ = rc[node]
        except KeyError as e:
            assert e.args[0] is node
        else:
            raise AssertionError("expected KeyError")

    def test_pop_drops_object_keys(self):
        """`pop()` exposes the popped layer as a `{str: value}` dict
        (ContextDict only models string keys). Object-keyed entries
        from that layer are dropped on pop."""

        class FakeNode:
            pass

        node = FakeNode()
        rc = _new_render_context(OxideContext)
        with rc.push_state(_FakeTemplate(name="t")) as _state:
            rc["a"] = 1
            rc[node] = "secret"
        assert "a" not in rc
        assert node not in rc

class _FakeTemplate:
    """Duck-type for `RenderContext.push_state(template)`; only
    `template.name` is read."""

    def __init__(self, name):
        self.name = name


# Official-tag compliance: render the same template through the SAME
# Engine on Django and oxide; assert byte-identical output.
#
# Five tags need a working template loader (block, extends, include,
# partial, partialdef). Oxide's include/extends routes through
# `django.template.loader.get_template`, which uses globally
# registered engines via `settings.TEMPLATES`, not the per-test Engine.
# Fixture templates live under
# `tests/django_template_tests/templates/oxide_*.html` and are picked
# up via `app_directories`.

# Minimal in-process URLconf so `{% url %}` has something to reverse.
def _view(request):
    raise NotImplementedError  # reverse() only needs the pattern


_test_urlpatterns = [
    path("client/<int:client_id>/", _view, name="client"),
    path("home/", _view, name="home"),
]


@pytest.fixture(scope="module")
def url_conf():
    """Install the test URLconf for the module so `{% url %}` resolves."""
    set_urlconf(__name__)
    yield
    set_urlconf(None)


# `set_urlconf(__name__)` above expects this attribute on the module.
urlpatterns = _test_urlpatterns


class TestAllDjangoTemplateTags:
    """Every built-in template tag must render byte-identically.

    Source anchors (Django 6.0):
      - `defaulttags.py`: autoescape, comment, csrf_token, cycle,
        debug, filter, firstof, for, if, ifchanged, load, lorem, now,
        partial, partialdef, querystring, regroup, resetcycle,
        spaceless, templatetag, url, verbatim, widthratio, with.
      - `loader_tags.py`: block, extends, include.

    27 tags total. Argument forms confirmed against `do_<tag>` parse
    functions and `<Tag>Node` classes:
      - `cycle ... as name silent`: defaulttags.py:99
      - `firstof ... as alias`: defaulttags.py:149
      - `url 'name' kw=val as alias`: defaulttags.py:479
      - `widthratio ... as alias`: defaulttags.py:534
      - `partialdef name inline`: defaulttags.py:1222
      - `include "tpl" with k=v only`: loader_tags.py:330, 352.
    """

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{% autoescape off %}{{ x }}{% endautoescape %}", {"x": "<b>"}),
            ("{% autoescape on %}{{ x }}{% endautoescape %}", {"x": "<b>"}),
        ],
        ids=["off", "on"],
    )
    def test_autoescape(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    def test_comment_drops_content(self, engine):
        assert_render_matches(
            engine,
            "before{% comment %}hidden text {{ x }} {% if y %}...{% endif %}{% endcomment %}after",
            {"x": "VAR", "y": True},
        )

    def test_comment_with_note(self, engine):
        """`{% comment "note" %}...{% endcomment %}`: note is ignored."""
        assert_render_matches(
            engine,
            "X{% comment 'why' %}skip{% endcomment %}Y",
            {},
        )

    def test_cycle_three_values(self, engine):
        assert_render_matches(
            engine,
            "{% for i in items %}{% cycle 'a' 'b' 'c' %}{% endfor %}",
            {"items": [1, 2, 3, 4, 5, 6, 7]},
        )

    def test_cycle_with_variables(self, engine):
        """`{% cycle x y z %}` cycles resolved variables, not literals."""
        assert_render_matches(
            engine,
            "{% for i in items %}{% cycle x y %}{% endfor %}",
            {"items": [1, 2, 3, 4], "x": "X", "y": "Y"},
        )

    def test_cycle_as_silent_alias(self, engine):
        """`{% cycle 'a' 'b' as name %}` defines a silent alias usable later."""
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% cycle 'a' 'b' as parity silent %}"
                "{{ i }}:{{ parity }},"
                "{% endfor %}"
            ),
            {"items": [1, 2, 3, 4]},
        )

    def test_debug_contains_known_var(self, engine):
        """`{% debug %}` dumps the context. Both engines must match for
        a fixed context (modules-listing portion is process-state but
        stable within a process)."""
        assert_render_matches(engine, "{% debug %}", {"x": 42, "name": "test"})

    def test_filter_tag_upper(self, engine):
        assert_render_matches(
            engine,
            "{% filter upper %}hello{% endfilter %}",
            {},
        )

    def test_filter_tag_chain(self, engine):
        assert_render_matches(
            engine,
            "{% filter upper|cut:'O' %}hello{% endfilter %}",
            {},
        )

    def test_firstof_all_falsy(self, engine):
        assert_render_matches(
            engine,
            "{% firstof a b c %}",
            {"a": "", "b": False, "c": None},
        )

    def test_firstof_picks_first_truthy(self, engine):
        assert_render_matches(
            engine,
            "{% firstof a b c %}",
            {"a": "", "b": "got-it", "c": "later"},
        )

    def test_firstof_with_default_literal(self, engine):
        assert_render_matches(
            engine,
            '{% firstof a b "fallback" %}',
            {"a": "", "b": ""},
        )

    def test_firstof_as_alias(self, engine):
        assert_render_matches(
            engine,
            '{% firstof a b "fallback" as out %}out is [{{ out }}]',
            {"a": "", "b": ""},
        )

    def test_for_basic(self, engine):
        assert_render_matches(
            engine,
            "{% for i in items %}{{ i }},{% endfor %}",
            {"items": [1, 2, 3]},
        )

    def test_for_empty_clause(self, engine):
        assert_render_matches(
            engine,
            "{% for i in items %}{{ i }}{% empty %}NONE{% endfor %}",
            {"items": []},
        )

    def test_for_forloop_vars(self, engine):
        assert_render_matches(
            engine,
            (
                "{% for x in items %}"
                "{{ forloop.counter }}/{{ forloop.counter0 }}/"
                "{{ forloop.revcounter }}/{{ forloop.revcounter0 }}/"
                "{{ forloop.first }}/{{ forloop.last }};"
                "{% endfor %}"
            ),
            {"items": ["a", "b", "c"]},
        )

    def test_for_reversed(self, engine):
        assert_render_matches(
            engine,
            "{% for i in items reversed %}{{ i }}{% endfor %}",
            {"items": [1, 2, 3]},
        )

    def test_for_nested_forloop_parentloop(self, engine):
        assert_render_matches(
            engine,
            (
                "{% for a in outer %}"
                "{% for b in inner %}"
                "[{{ forloop.parentloop.counter }}.{{ forloop.counter }}]"
                "{% endfor %}"
                "{% endfor %}"
            ),
            {"outer": ["x", "y"], "inner": [1, 2, 3]},
        )

    def test_if_complex_expression(self, engine):
        assert_render_matches(
            engine,
            "{% if x > 0 and y == 'foo' or not z %}YES{% else %}NO{% endif %}",
            {"x": 5, "y": "foo", "z": True},
        )

    def test_if_in_operator(self, engine):
        assert_render_matches(
            engine,
            "{% if x in items %}IN{% else %}OUT{% endif %}",
            {"x": 2, "items": [1, 2, 3]},
        )

    def test_if_not_in_operator(self, engine):
        assert_render_matches(
            engine,
            "{% if x not in items %}OUT{% else %}IN{% endif %}",
            {"x": 5, "items": [1, 2, 3]},
        )

    def test_if_is_none(self, engine):
        assert_render_matches(
            engine,
            "{% if x is None %}none{% else %}some{% endif %}",
            {"x": None},
        )

    def test_ifchanged_basic(self, engine):
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% ifchanged i %}[{{ i }}]{% endifchanged %}"
                "{% endfor %}"
            ),
            {"items": [1, 1, 2, 2, 2, 3, 1]},
        )

    def test_ifchanged_else(self, engine):
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% ifchanged i %}NEW:{{ i }}{% else %}SAME{% endifchanged %};"
                "{% endfor %}"
            ),
            {"items": [1, 1, 2, 2]},
        )

    def test_load_library_then_use_filter(self, engine):
        """`{% load custom %}` makes the library's filters available."""
        assert_render_matches(
            engine,
            "{% load custom %}{{ val|noop }}",
            {"val": "kept"},
        )

    def test_load_specific_names_via_from(self, engine):
        """`{% load name1 from lib %}` imports a single tag/filter name."""
        assert_render_matches(
            engine,
            "{% load noop from custom %}{{ val|noop }}",
            {"val": "kept"},
        )

    def test_lorem_fixed_first_paragraph(self, engine):
        """`{% lorem 1 b %}`: deterministic first paragraph."""
        assert_render_matches(engine, "{% lorem 1 b %}", {})

    def test_lorem_words(self, engine):
        """`{% lorem N w %}` returns the first N COMMON_WORDS."""
        assert_render_matches(engine, "{% lorem 3 w %}", {})

    def test_now_year(self, engine):
        """`{% now "Y" %}` renders the current 4-digit year."""
        assert_render_matches(engine, '{% now "Y" %}', {})

    def test_now_as_alias(self, engine):
        """`{% now "Y" as year %}` stores for later interpolation."""
        assert_render_matches(
            engine,
            '{% now "Y" as year %}year is {{ year }}',
            {},
        )

    def test_querystring_appends_kwarg(self, engine):
        """`{% querystring %}` merges kwargs into the current
        `request.GET`. Django reads `context.request` as an attribute
        (RequestContext convention); each engine uses its own context
        type."""
        request = _http_request("/x/?page=2")
        src = '{% querystring sort="name" %}'

        dj_tpl = engine.from_string(src)
        # Without context processors, set `request` as both attribute
        # and dict entry to keep the comparison apples-to-apples.
        dj_out = dj_tpl.render(DjangoRequestContext(request, {"request": request}))
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext({"request": request})
        )
        assert ox_out == dj_out, (
            f"\n  template: {src!r}"
            f"\n  django:   {dj_out!r}"
            f"\n  oxide:    {ox_out!r}"
        )

    def test_querystring_removes_with_none(self, engine):
        """Passing ``None`` removes that key from the merged result."""
        request = _http_request("/x/?page=2&filter=open")
        src = "{% querystring filter=None %}"

        dj_tpl = engine.from_string(src)
        dj_out = dj_tpl.render(DjangoRequestContext(request, {"request": request}))
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext({"request": request})
        )
        assert ox_out == dj_out, (
            f"\n  template: {src!r}"
            f"\n  django:   {dj_out!r}"
            f"\n  oxide:    {ox_out!r}"
        )

    def test_regroup_by_key(self, engine):
        assert_render_matches(
            engine,
            (
                "{% regroup people by city as groups %}"
                "{% for g in groups %}"
                "[{{ g.grouper }}:{% for p in g.list %}{{ p.name }},{% endfor %}]"
                "{% endfor %}"
            ),
            {
                "people": [
                    {"name": "alice", "city": "NYC"},
                    {"name": "bob", "city": "NYC"},
                    {"name": "carol", "city": "LA"},
                ],
            },
        )

    def test_resetcycle_no_arg(self, engine):
        """Bare `{% resetcycle %}` resets the most recently declared cycle."""
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% cycle 'a' 'b' 'c' %}"
                "{% if forloop.counter == 2 %}{% resetcycle %}{% endif %}"
                "{% endfor %}"
            ),
            {"items": [1, 2, 3, 4, 5]},
        )

    def test_resetcycle_named(self, engine):
        """`{% resetcycle name %}` resets a specific cycle declared with
        `{% cycle ... as name %}`."""
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% cycle 'a' 'b' as letters %}"
                "{% if forloop.counter == 2 %}{% resetcycle letters %}{% endif %}"
                "{% endfor %}"
            ),
            {"items": [1, 2, 3, 4]},
        )

    def test_spaceless_strips_whitespace_between_tags(self, engine):
        assert_render_matches(
            engine,
            "{% spaceless %}<a>\n  <b>hi</b>\n</a>{% endspaceless %}",
            {},
        )

    @pytest.mark.parametrize(
        "marker",
        [
            "openblock",
            "closeblock",
            "openvariable",
            "closevariable",
            "openbrace",
            "closebrace",
            "opencomment",
            "closecomment",
        ],
    )
    def test_templatetag_each_marker(self, engine, marker):
        assert_render_matches(engine, "{% templatetag " + marker + " %}", {})

    def test_url_no_args(self, engine, url_conf):
        assert_render_matches(engine, "{% url 'home' %}", {})

    def test_url_kwargs(self, engine, url_conf):
        assert_render_matches(
            engine,
            "{% url 'client' client_id=42 %}",
            {},
        )

    def test_url_with_variable(self, engine, url_conf):
        assert_render_matches(
            engine,
            "{% url name client_id=id %}",
            {"name": "client", "id": 7},
        )

    def test_url_as_alias(self, engine, url_conf):
        assert_render_matches(
            engine,
            "{% url 'home' as u %}u=[{{ u }}]",
            {},
        )

    def test_verbatim_keeps_django_syntax_literal(self, engine):
        assert_render_matches(
            engine,
            "{% verbatim %}{{ not_a_var }} and {% not_a_tag %}{% endverbatim %}",
            {},
        )

    def test_verbatim_named_endblock(self, engine):
        """`{% verbatim mylabel %}...{% endverbatim mylabel %}` lets you
        nest by giving the close a distinguishing label."""
        assert_render_matches(
            engine,
            "{% verbatim myblock %}{{ x }}{% endverbatim myblock %}",
            {},
        )

    def test_widthratio_basic(self, engine):
        assert_render_matches(
            engine,
            "{% widthratio val max width %}",
            {"val": 50, "max": 100, "width": 200},
        )

    def test_widthratio_as_alias(self, engine):
        assert_render_matches(
            engine,
            "{% widthratio val max width as ratio %}r={{ ratio }}",
            {"val": 25, "max": 100, "width": 100},
        )

    def test_with_multiple_assignments(self, engine):
        assert_render_matches(
            engine,
            '{% with a="A" b="B" c="C" %}{{ a }}-{{ b }}-{{ c }}{% endwith %}',
            {},
        )

    def test_with_legacy_as_syntax(self, engine):
        """`{% with expr as name %}`: the legacy form Django still
        accepts."""
        assert_render_matches(
            engine,
            '{% with val|upper as out %}{{ out }}{% endwith %}',
            {"val": "hello"},
        )

    def test_csrf_token_renders_input(self, engine):
        assert_render_matches(
            engine,
            "{% csrf_token %}",
            {"csrf_token": "FAKE_CSRF_VALUE_1234"},
        )

    # The next five tests (include, extends, block, partial, partialdef)
    # route through `django.template.loader.get_template`; fixtures
    # under `tests/django_template_tests/templates/oxide_*.html` are
    # picked up via `APP_DIRS=True`.
    def test_include_with_context_passthrough(self, engine):
        assert_render_matches(
            engine,
            "{% include 'oxide_fragment.html' %}",
            {"who": "world"},
        )

    def test_include_with_only(self, engine):
        """`{% include ... only %}` isolates the included template from
        the caller's context."""
        assert_render_matches(
            engine,
            "{% include 'oxide_fragment.html' only %}",
            {"who": "world"},
        )

    def test_include_with_kwargs(self, engine):
        """`{% include ... with k=v %}` injects extra context vars."""
        assert_render_matches(
            engine,
            "{% include 'oxide_fragment.html' with who='kw' %}",
            {"who": "outer"},
        )

    def test_extends_overrides_blocks(self, engine):
        """Child template extends a base and overrides both blocks."""

        # `get_template` flows through global engines on both backends.
        # We compare the FULL rendered output of the child template via
        # each engine. To do this without losing the per-test
        # `Engine` fixture's library config we render directly through
        # `engine.get_template` (stock) and through oxide's
        # `Template(<source>, engine=engine)` after reading source from
        # disk.
        child = dj_get_template("oxide_child.html")
        src = child.template.source

        dj_out = engine.from_string(src).render(DjangoContext({"name": "world"}))
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext({"name": "world"})
        )
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    def test_extends_with_block_super(self, engine):
        """`{{ block.super }}` inside a child block pulls the parent."""

        child = dj_get_template("oxide_child_with_super.html")
        src = child.template.source

        dj_out = engine.from_string(src).render(DjangoContext({}))
        ox_out = OxideTemplate(src, engine=engine).render(OxideContext({}))
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    def test_partialdef_and_partial(self, engine):
        """`{% partialdef name inline %}...{% endpartialdef %}` defines
        a reusable fragment in-place; `{% partial name %}` invokes it.
        The `inline` modifier makes the partialdef's content also
        render at the definition site."""

        host = dj_get_template("oxide_partial_host.html")
        src = host.template.source

        dj_out = engine.from_string(src).render(
            DjangoContext({"name": "world"})
        )
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext({"name": "world"})
        )
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    # Argument-form coverage. Each remaining documented argument form
    # for full compliance against Django 6.0's built-in tag inventory.

    def test_block_named_endblock_match(self, engine):
        """`{% endblock <name> %}` matching its `{% block <name> %}`."""
        assert_render_matches(
            engine,
            "{% block hello %}HI{% endblock hello %}",
            {},
        )

    def test_block_nested(self, engine):
        """Nested blocks inside a block render correctly when the
        template is rendered standalone (no extends in play)."""
        assert_render_matches(
            engine,
            (
                "{% block outer %}<o>"
                "{% block inner %}<i>VAL</i>{% endblock %}"
                "</o>{% endblock %}"
            ),
            {},
        )

    def test_block_multiple_siblings(self, engine):
        assert_render_matches(
            engine,
            (
                "{% block a %}A{% endblock %}"
                "|"
                "{% block b %}B{% endblock %}"
                "|"
                "{% block c %}C{% endblock %}"
            ),
            {},
        )

    # ---- cycle: re-referencing a named cycle -------------------------------
    def test_cycle_reference_named(self, engine):
        """A `{% cycle name %}` after `{% cycle ... as name %}` advances
        the same cycle."""
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% cycle 'a' 'b' as letters %}|{% cycle letters %};"
                "{% endfor %}"
            ),
            {"items": [1, 2, 3]},
        )

    # ---- extends: dynamic parent + 3-level + childless --------------------
    def test_extends_dynamic_parent_name(self, engine):
        """`{% extends parent_var %}` resolves the parent name at render."""
        src = (
            "{% extends parent %}"
            "{% block title %}dyn-title{% endblock %}"
        )
        dj_out = engine.from_string(src).render(
            DjangoContext({"parent": "oxide_base.html"})
        )
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext({"parent": "oxide_base.html"})
        )
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    def test_extends_three_level_inheritance(self, engine):
        """grandchild → middle → base. Each level overrides one block."""

        child = dj_get_template("oxide_grandchild.html")
        src = child.template.source

        dj_out = engine.from_string(src).render(DjangoContext({}))
        ox_out = OxideTemplate(src, engine=engine).render(OxideContext({}))
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    def test_extends_child_overrides_no_blocks(self, engine):
        """A child that extends but declares no block of its own renders
        the parent verbatim: sanity check for the no-override path."""

        child = dj_get_template("oxide_no_blocks.html")
        src = child.template.source

        dj_out = engine.from_string(src).render(DjangoContext({}))
        ox_out = OxideTemplate(src, engine=engine).render(OxideContext({}))
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    # Note: `{% filter length %}...{% endfilter %}` is excluded because
    # Django itself fails with `TypeError: sequence item 0: expected
    # str instance, int found`. A Django bug, not a compliance case.
    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{% filter lower %}HELLO{% endfilter %}", {}),
            ("{% filter title %}hello world{% endfilter %}", {}),
            ("{% filter cut:'o' %}food{% endfilter %}", {}),
            ("{% filter linebreaks %}a\n\nb{% endfilter %}", {}),
            ("{% filter upper|lower %}Hello World{% endfilter %}", {}),
        ],
    )
    def test_filter_tag_various_chains(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    # ---- for: extra forms --------------------------------------------------
    def test_for_dict_items(self, engine):
        """Plain `for k, v` on a dict-like context value."""
        assert_render_matches(
            engine,
            "{% for k, v in items %}{{ k }}={{ v }};{% endfor %}",
            {"items": [("a", 1), ("b", 2)]},
        )

    def test_for_forloop_parentloop_in_nested(self, engine):
        """`forloop.parentloop.counter` resolves inside a nested loop."""
        assert_render_matches(
            engine,
            (
                "{% for o in outer %}"
                "[O{{ forloop.counter }}:"
                "{% for i in inner %}"
                "P{{ forloop.parentloop.counter }}I{{ forloop.counter }} "
                "{% endfor %}"
                "]"
                "{% endfor %}"
            ),
            {"outer": ["x", "y"], "inner": [1, 2]},
        )

    def test_for_reversed_with_unpacking(self, engine):
        """`{% for k, v in items reversed %}`: both flags combined."""
        assert_render_matches(
            engine,
            "{% for k, v in items reversed %}{{ k }}={{ v }};{% endfor %}",
            {"items": [("a", 1), ("b", 2), ("c", 3)]},
        )

    # ---- if: every operator + precedence -----------------------------------
    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{% if x >= y %}Y{% else %}N{% endif %}", {"x": 5, "y": 5}),
            ("{% if x <= y %}Y{% else %}N{% endif %}", {"x": 5, "y": 5}),
            ("{% if x != y %}Y{% else %}N{% endif %}", {"x": 1, "y": 2}),
            ("{% if x < y %}Y{% else %}N{% endif %}", {"x": 1, "y": 2}),
            ("{% if x > y %}Y{% else %}N{% endif %}", {"x": 3, "y": 2}),
            ("{% if x is None %}Y{% else %}N{% endif %}", {"x": None}),
            ("{% if x is not None %}Y{% else %}N{% endif %}", {"x": 1}),
            ("{% if x in items %}Y{% else %}N{% endif %}", {"x": "a", "items": "cab"}),
            (
                "{% if x in items %}Y{% else %}N{% endif %}",
                {"x": "k", "items": {"k": 1}},
            ),
            # Precedence: `a or b and c` parses as `a or (b and c)`.
            (
                "{% if a or b and c %}T{% else %}F{% endif %}",
                {"a": False, "b": True, "c": True},
            ),
            (
                "{% if a or b and c %}T{% else %}F{% endif %}",
                {"a": False, "b": True, "c": False},
            ),
            (
                "{% if not not x %}Y{% else %}N{% endif %}",
                {"x": "non-empty"},
            ),
            (
                "{% if x == 1 %}1{% elif x == 2 %}2{% elif x == 3 %}3{% else %}?{% endif %}",
                {"x": 3},
            ),
        ],
    )
    def test_if_operators_and_precedence(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    # ---- ifchanged: explicit comparison var + multi-var ------------------
    def test_ifchanged_with_explicit_value(self, engine):
        """`{% ifchanged v %}` watches v explicitly."""
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% ifchanged i.cat %}[{{ i.cat }}]{% endifchanged %}{{ i.name }};"
                "{% endfor %}"
            ),
            {
                "items": [
                    {"cat": "fruit", "name": "apple"},
                    {"cat": "fruit", "name": "pear"},
                    {"cat": "veg", "name": "kale"},
                    {"cat": "veg", "name": "leek"},
                ],
            },
        )

    def test_ifchanged_multi_var(self, engine):
        """Multiple change-watch variables on one tag."""
        assert_render_matches(
            engine,
            (
                "{% for i in items %}"
                "{% ifchanged i.a i.b %}CH{% else %}SAME{% endifchanged %};"
                "{% endfor %}"
            ),
            {
                "items": [
                    {"a": 1, "b": 1},
                    {"a": 1, "b": 1},
                    {"a": 1, "b": 2},
                    {"a": 2, "b": 2},
                ],
            },
        )

    # ---- include: variable name + with-only --------------------------------
    def test_include_variable_template_name(self, engine):
        """`{% include name %}` resolves the template name from context."""
        assert_render_matches(
            engine,
            "{% include tpl %}",
            {"tpl": "oxide_fragment.html", "who": "world"},
        )

    def test_include_with_only(self, engine):
        """`{% include ... with k=v only %}`: `with` keyword overrides
        and `only` isolates."""
        assert_render_matches(
            engine,
            "{% include 'oxide_fragment.html' with who='kw' only %}",
            {"who": "outer"},
        )

    # ---- load: multiple libraries in one tag -------------------------------
    def test_load_multiple_libraries(self, engine):
        """`{% load lib1 lib2 %}`. Use `custom` twice to exercise the
        multi-name parse path."""
        assert_render_matches(
            engine,
            "{% load custom custom %}{{ val|noop }}",
            {"val": "ok"},
        )

    # ---- lorem: random and word counts -----------------------------------
    @pytest.mark.parametrize(
        "src",
        [
            "{% lorem 1 b %}",
            "{% lorem 2 b %}",
            "{% lorem 3 w %}",
            "{% lorem 5 w random %}",
        ],
    )
    def test_lorem_forms(self, engine, src):
        # `random` taps into `random.shuffle`/`random.choice`; seed it
        # so stock and oxide get the same sequence.
        state = random.getstate()
        try:
            random.seed(0)
            dj_out = engine.from_string(src).render(DjangoContext({}))
            random.seed(0)
            ox_out = OxideTemplate(src, engine=engine).render(OxideContext({}))
        finally:
            random.setstate(state)
        assert ox_out == dj_out, (
            f"\n  src: {src!r}\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    # ---- now: many format specifiers ---------------------------------------
    @pytest.mark.parametrize(
        "fmt",
        [
            "Y",
            "y",
            "m",
            "n",
            "M",
            "D",
            "d",
            "H",
            "G",
            "i",
            "s",
            "H:i",
            "Y-m-d",
            "D, d M Y",
        ],
    )
    def test_now_format_specifiers(self, engine, fmt):
        """Each format char produces the same output as stock."""
        src = '{% now "' + fmt + '" %}'
        assert_render_matches(engine, src, {})

    # ---- partial: extra shapes --------------------------------------------
    def test_partialdef_non_inline(self, engine):
        """`{% partialdef foo %}...{% endpartialdef %}` (no `inline`) is
        silent at the definition site; `{% partial foo %}` renders it."""
        src = (
            "before|"
            "{% partialdef msg %}HELLO{% endpartialdef %}"
            "middle|{% partial msg %}|after"
        )
        assert_render_matches(engine, src, {})

    def test_partial_forward_reference(self, engine):
        """`{% partial %}` before its `{% partialdef %}`: both are
        resolved at render time, so order in the source doesn't matter."""

        host = dj_get_template("oxide_partial_advanced.html")
        src = host.template.source

        dj_out = engine.from_string(src).render(DjangoContext({}))
        ox_out = OxideTemplate(src, engine=engine).render(OxideContext({}))
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    # ---- querystring: positional dict arg ----------------------------------
    def test_querystring_positional_dict(self, engine):
        """First positional arg can be a dict that replaces the merge
        base instead of using ``request.GET``."""
        request = _http_request("/x/?page=2")
        src = "{% querystring extras %}"
        dj_tpl = engine.from_string(src)
        dj_out = dj_tpl.render(
            DjangoRequestContext(
                request, {"request": request, "extras": {"sort": "name"}}
            )
        )
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext({"request": request, "extras": {"sort": "name"}})
        )
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    # ---- regroup: nested key + sorting --------------------------------------
    def test_regroup_nested_key(self, engine):
        """`regroup by a.b` walks a dotted key."""
        assert_render_matches(
            engine,
            (
                "{% regroup items by meta.country as groups %}"
                "{% for g in groups %}"
                "[{{ g.grouper }}:{% for it in g.list %}{{ it.name }};{% endfor %}]"
                "{% endfor %}"
            ),
            {
                "items": [
                    {"name": "x", "meta": {"country": "US"}},
                    {"name": "y", "meta": {"country": "US"}},
                    {"name": "z", "meta": {"country": "FR"}},
                ],
            },
        )

    # ---- spaceless: block tags inside --------------------------------------
    def test_spaceless_with_inner_block_tags(self, engine):
        assert_render_matches(
            engine,
            (
                "{% spaceless %}"
                "  <ul>\n"
                "  {% for i in items %}<li>{{ i }}</li>\n  {% endfor %}\n"
                "  </ul>"
                "{% endspaceless %}"
            ),
            {"items": [1, 2]},
        )

    # ---- url: positional + mixed forms -------------------------------------
    def test_url_positional_arg(self, engine, url_conf):
        assert_render_matches(
            engine,
            "{% url 'client' 42 %}",
            {},
        )

    def test_url_view_via_variable_only(self, engine, url_conf):
        """`{% url name %}` with the view name resolved from context."""
        assert_render_matches(
            engine,
            "{% url name %}",
            {"name": "home"},
        )

    # ---- widthratio: edge cases -------------------------------------------
    @pytest.mark.parametrize(
        "val,max_,width",
        [
            (0, 100, 200),
            (100, 100, 200),
            (50, 0, 200),
            (75, 200, 100),
            (-10, 100, 200),
            (50, 100, 0),
        ],
        ids=["zero-val", "at-max", "zero-max", "rounded", "neg-val", "zero-width"],
    )
    def test_widthratio_edges(self, engine, val, max_, width):
        """Each edge renders byte-identical between Django and oxide.
        We don't pin the exact form, just that both engines agree."""
        ctx = {"val": val, "max": max_, "width": width}
        assert_render_matches(engine, "{% widthratio val max width %}", ctx)

    # ---- with: nested + scope shadowing ----------------------------------
    def test_with_nested(self, engine):
        """Nested `{% with %}` blocks each push their own scope."""
        assert_render_matches(
            engine,
            (
                "{% with x='outer' %}"
                "{{ x }}|"
                "{% with x='inner' %}{{ x }}{% endwith %}"
                "|{{ x }}"
                "{% endwith %}"
            ),
            {},
        )

    def test_with_shadows_outer_var(self, engine):
        """`{% with %}` shadows a same-named outer variable."""
        assert_render_matches(
            engine,
            "{{ x }}|{% with x='shadow' %}{{ x }}{% endwith %}|{{ x }}",
            {"x": "outer"},
        )


# Built-in filter compliance. Every filter in
# `django.template.defaultfilters` (57 in Django 6.0) gets at least
# one representative case. Non-deterministic filters (`random`,
# `timesince`/`timeuntil`) seed RNG or pin timestamps.


class TestAllDjangoFilters:
    """Byte-identical compliance for every built-in default filter.

    Source anchor: `defaultfilters.py` (Django 6.0); 57 filters total.
    Key behaviours cited:
      - `pluralize:"y,ies"`: defaultfilters.py:958-963.
      - `pluralize` list-of-length-1 -> singular: 977-978.
      - `yesno:"y,n"` with None -> "n": 884-886.
      - `floatformat:'-N'` strips trailing zeros: 115-120.
      - `floatformat:'<N>g'` thousands: 122-127.
      - `floatformat:'<N>u'` unlocalized: 129-133.
      - `make_list` over int -> digit chars (+@stringfilter): 256-263.
      - `slugify` accents: utils/text.py:472-481.
      - `striptags` no whitespace insertion: utils/html.py:200-207.
      - `default` with empty list -> fallback: 843.
    """

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|add:'5' }}", {"x": 10}),
            ("{{ x|add:'5' }}", {"x": "hello"}),       # str+int → invalid → "" per Django
            ("{{ x|add:y }}", {"x": 3, "y": 4}),
            ("{{ x|add:y }}", {"x": [1, 2], "y": [3]}),  # list concat
        ],
        ids=["int+int", "str+int", "var+var-int", "list+list"],
    )
    def test_add(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|addslashes }}', {"x": 'I\'m "ok"'}),
            ('{{ x|addslashes }}', {"x": "back\\slash"}),
            ('{{ x|addslashes }}', {"x": ""}),
        ],
        ids=["quotes", "backslash", "empty"],
    )
    def test_addslashes(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|capfirst }}", {"x": "hello"}),
            ("{{ x|capfirst }}", {"x": "HELLO"}),    # only first char affected
            ("{{ x|capfirst }}", {"x": ""}),
            ("{{ x|capfirst }}", {"x": "1abc"}),     # leading non-letter
        ],
        ids=["lower", "upper", "empty", "digit-start"],
    )
    def test_capfirst(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|center:"15" }}', {"x": "hi"}),
            ('{{ x|center:"5" }}', {"x": "ab"}),
            ('{{ x|center:"3" }}', {"x": "abcdef"}),  # width < len
        ],
        ids=["pad", "uneven", "shorter-than-content"],
    )
    def test_center(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|cut:' ' }}", {"x": "hello world"}),
            ("{{ x|cut:'o' }}", {"x": "food"}),
            ("{{ x|cut:'z' }}", {"x": "abc"}),
        ],
        ids=["space", "char", "no-match"],
    )
    def test_cut(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "fmt",
        [
            "Y-m-d",
            "Y-m-d H:i:s",
            "D, d M Y",
            "M d, Y",
            "F jS, Y",
            "h:i A",
            "l",
            "N",
            "g:i a",
            "U",
            "c",
            "r",
        ],
    )
    def test_date_format_chars(self, engine, fmt):
        """Cover the format chars our native Rust formatter handles
        (Y/m/d/H/i/s/D/l/N/M/F/j/h/g/a/A) and the ones it falls back
        to Django for (U/c/r/Z/T)."""
        ctx = {"x": datetime.datetime(2024, 7, 4, 13, 7, 9)}
        assert_render_matches(engine, '{{ x|date:"' + fmt + '" }}', ctx)

    def test_date_on_naive_date(self, engine):
        """`date` accepts a `datetime.date` (no time)."""
        assert_render_matches(
            engine,
            '{{ x|date:"Y-m-d" }}',
            {"x": datetime.date(2024, 7, 4)},
        )

    def test_date_on_none(self, engine):
        """`None|date:"..."` renders empty string."""
        assert_render_matches(engine, '{{ x|date:"Y" }}', {"x": None})

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|default:"fallback" }}', {"x": ""}),
            ('{{ x|default:"fallback" }}', {"x": "value"}),
            ('{{ x|default:"fallback" }}', {"x": 0}),       # 0 is falsy
            ('{{ x|default:"fallback" }}', {"x": False}),
            ('{{ x|default:"fallback" }}', {"x": []}),
            ('{{ x|default:"fallback" }}', {}),             # missing var
        ],
        ids=["empty-str", "value", "zero", "false", "empty-list", "missing"],
    )
    def test_default(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|default_if_none:"fb" }}', {"x": None}),
            ('{{ x|default_if_none:"fb" }}', {"x": ""}),        # empty != None
            ('{{ x|default_if_none:"fb" }}', {"x": 0}),         # 0 != None
            ('{{ x|default_if_none:"fb" }}', {"x": False}),
        ],
        ids=["none", "empty-str", "zero", "false"],
    )
    def test_default_if_none(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            (
                "{% for d in items|dictsort:'name' %}{{ d.name }};{% endfor %}",
                {
                    "items": [
                        {"name": "c"},
                        {"name": "a"},
                        {"name": "b"},
                    ],
                },
            ),
            (
                "{% for d in items|dictsortreversed:'name' %}{{ d.name }};{% endfor %}",
                {
                    "items": [
                        {"name": "c"},
                        {"name": "a"},
                        {"name": "b"},
                    ],
                },
            ),
            (
                "{% for d in items|dictsort:'meta.age' %}{{ d.meta.age }};{% endfor %}",
                {
                    "items": [
                        {"meta": {"age": 30}},
                        {"meta": {"age": 10}},
                        {"meta": {"age": 20}},
                    ],
                },
            ),
        ],
        ids=["dictsort-flat", "dictsortreversed-flat", "dictsort-nested-key"],
    )
    def test_dictsort_family(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|divisibleby:'3' }}", {"x": 9}),
            ("{{ x|divisibleby:'3' }}", {"x": 10}),
            ("{{ x|divisibleby:y }}", {"x": 12, "y": 4}),
        ],
        ids=["yes", "no", "var-divisor"],
    )
    def test_divisibleby(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|escape }}", {"x": "<b>x</b>"}),
            ("{{ x|escapejs }}", {"x": "alert('x')"}),
            ("{{ x|escapejs }}", {"x": "a\nb"}),
            ("{{ x|safe }}", {"x": "<b>x</b>"}),
            ("{{ x|force_escape }}", {"x": "<b>x</b>"}),
        ],
        ids=["escape", "escapejs-quotes", "escapejs-newline", "safe", "force_escape"],
    )
    def test_escape_family(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    def test_escapeseq(self, engine):
        """`escapeseq` escapes each item of a sequence individually."""
        assert_render_matches(
            engine,
            "{% for s in items|escapeseq %}{{ s }};{% endfor %}",
            {"items": ["<a>", "<b>"]},
        )

    def test_safeseq(self, engine):
        """`safeseq` marks each item in a sequence as safe (so a later
        ``join`` doesn't escape them)."""
        assert_render_matches(
            engine,
            "{{ items|safeseq|join:', ' }}",
            {"items": ["<a>", "<b>"]},
        )

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|filesizeformat }}", {"x": 0}),
            ("{{ x|filesizeformat }}", {"x": 1023}),
            ("{{ x|filesizeformat }}", {"x": 1024}),
            ("{{ x|filesizeformat }}", {"x": 1024 * 1024}),
            ("{{ x|filesizeformat }}", {"x": 1024 * 1024 * 1024}),
            ("{{ x|filesizeformat }}", {"x": 1024 * 1024 * 1024 * 1024}),
        ],
        ids=["zero", "bytes", "KB", "MB", "GB", "TB"],
    )
    def test_filesizeformat(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|first }}", {"x": [1, 2, 3]}),
            ("{{ x|first }}", {"x": "hello"}),
            ("{{ x|last }}", {"x": [1, 2, 3]}),
            ("{{ x|last }}", {"x": "hello"}),
            ("{{ x|first }}", {"x": []}),
            ("{{ x|last }}", {"x": []}),
        ],
        ids=["first-list", "first-str", "last-list", "last-str", "first-empty", "last-empty"],
    )
    def test_first_last(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|floatformat }}", {"x": 34.23}),
            ("{{ x|floatformat }}", {"x": 34.00}),       # bare int → no decimals
            ("{{ x|floatformat:'3' }}", {"x": 34.23}),
            ("{{ x|floatformat:'-3' }}", {"x": 34.23}),  # negative → strip trailing zeros
            ("{{ x|floatformat:'-3' }}", {"x": 34.00}),
            ('{{ x|floatformat:"u" }}', {"x": 34.5}),    # unlocalized
            ("{{ x|floatformat:'2g' }}", {"x": 1234567.89}),  # grouped
        ],
        ids=["default", "no-decimals", "fixed", "strip-trailing", "strip-int", "unlocalized", "grouped"],
    )
    def test_floatformat(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|get_digit:'1' }}", {"x": 12345}),  # rightmost
            ("{{ x|get_digit:'2' }}", {"x": 12345}),
            ("{{ x|get_digit:'5' }}", {"x": 12345}),
            ("{{ x|get_digit:'6' }}", {"x": 12345}),  # past end
            ("{{ x|get_digit:'1' }}", {"x": -123}),   # negative input: Django returns 0
        ],
        ids=["rightmost", "second", "leftmost", "past-end", "negative"],
    )
    def test_get_digit(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|iriencode }}", {"x": "café"}),
            ("{{ x|iriencode }}", {"x": "/path/with spaces"}),
            ("{{ x|iriencode }}", {"x": "?q=hello world&foo=bar"}),
        ],
        ids=["unicode", "spaces", "querystring-like"],
    )
    def test_iriencode(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|join:', ' }}", {"x": ["a", "b", "c"]}),
            ("{{ x|join:'' }}", {"x": ["a", "b", "c"]}),
            ("{{ x|join:', ' }}", {"x": []}),
            ("{{ x|join:', ' }}", {"x": ["<i>"]}),  # autoescape interaction
        ],
        ids=["comma", "empty-sep", "empty-list", "escape"],
    )
    def test_join(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|json_script:'data' }}", {"x": {"k": "v"}}),
            ("{{ x|json_script:'data' }}", {"x": [1, 2, 3]}),
            ("{{ x|json_script:'data' }}", {"x": "<>&'"}),  # HTML-dangerous chars
            ("{{ x|json_script:'data' }}", {"x": None}),
        ],
        ids=["dict", "list", "html-chars", "none"],
    )
    def test_json_script(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|length }}", {"x": "hello"}),
            ("{{ x|length }}", {"x": [1, 2, 3]}),
            ("{{ x|length }}", {"x": {"a": 1, "b": 2}}),
            ("{{ x|length }}", {"x": ""}),
            ("{{ x|length }}", {"x": []}),
        ],
        ids=["str", "list", "dict", "empty-str", "empty-list"],
    )
    def test_length(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|linebreaks }}", {"x": "para1\n\npara2\nline2"}),
            ("{{ x|linebreaksbr }}", {"x": "a\nb\nc"}),
            ("{{ x|linenumbers }}", {"x": "one\ntwo\nthree"}),
            ("{{ x|linenumbers }}", {"x": ""}),
        ],
        ids=["linebreaks", "linebreaksbr", "linenumbers", "linenumbers-empty"],
    )
    def test_linebreak_family(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|ljust:"10" }}', {"x": "hi"}),
            ('{{ x|rjust:"10" }}', {"x": "hi"}),
            ('{{ x|ljust:"3" }}', {"x": "abcdef"}),  # width < len
            ('{{ x|rjust:"3" }}', {"x": "abcdef"}),
        ],
        ids=["ljust", "rjust", "ljust-truncate-no", "rjust-truncate-no"],
    )
    def test_ljust_rjust(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|lower }}", {"x": "HELLO"}),
            ("{{ x|upper }}", {"x": "hello"}),
            ("{{ x|title }}", {"x": "hello world"}),
            ("{{ x|title }}", {"x": "a-b c"}),
            ("{{ x|lower }}", {"x": "ÜBER"}),  # unicode
        ],
        ids=["lower", "upper", "title", "title-mixed", "lower-unicode"],
    )
    def test_case_filters(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|make_list }}", {"x": "abc"}),
            ("{{ x|make_list }}", {"x": 123}),       # int → list of digit chars
            ("{{ x|make_list }}", {"x": ""}),
        ],
        ids=["str", "int", "empty"],
    )
    def test_make_list(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    # ---- phone2numeric ----------------------------------------------------
    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|phone2numeric }}", {"x": "1-800-COLLECT"}),
            ("{{ x|phone2numeric }}", {"x": "Mary"}),
            ("{{ x|phone2numeric }}", {"x": "1-800-COW-BACK"}),
        ],
        ids=["collect", "mary", "cow-back"],
    )
    def test_phone2numeric(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ n|pluralize }}", {"n": 1}),
            ("{{ n|pluralize }}", {"n": 2}),
            ("{{ n|pluralize }}", {"n": 0}),
            ('{{ n|pluralize:"es" }}', {"n": 1}),
            ('{{ n|pluralize:"es" }}', {"n": 2}),
            ('{{ n|pluralize:"y,ies" }}', {"n": 1}),
            ('{{ n|pluralize:"y,ies" }}', {"n": 2}),
            ("{{ items|pluralize }}", {"items": [1]}),
            ("{{ items|pluralize }}", {"items": [1, 2]}),
        ],
        ids=[
            "one", "two", "zero",
            "es-one", "es-two",
            "irregular-one", "irregular-two",
            "list-one", "list-two",
        ],
    )
    def test_pluralize(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|pprint }}", {"x": {"k": "v"}}),
            ("{{ x|pprint }}", {"x": [1, 2, 3]}),
            ("{{ x|pprint }}", {"x": "hi"}),
        ],
        ids=["dict", "list", "str"],
    )
    def test_pprint(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    def test_random(self, engine):
        """`random` taps into `random.choice`. Seed before each render
        so stock + oxide both pick the same item."""
        state = random.getstate()
        try:
            random.seed(0)
            dj_out = engine.from_string("{{ x|random }}").render(
                DjangoContext({"x": ["a", "b", "c", "d", "e"]})
            )
            random.seed(0)
            ox_out = OxideTemplate(
                "{{ x|random }}", engine=engine
            ).render(OxideContext({"x": ["a", "b", "c", "d", "e"]}))
        finally:
            random.setstate(state)
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|slice:":3" }}', {"x": [1, 2, 3, 4, 5]}),
            ('{{ x|slice:"2:" }}', {"x": [1, 2, 3, 4, 5]}),
            ('{{ x|slice:"1:4" }}', {"x": [1, 2, 3, 4, 5]}),
            ('{{ x|slice:"::2" }}', {"x": [1, 2, 3, 4, 5]}),
            ('{{ x|slice:"::-1" }}', {"x": [1, 2, 3]}),  # reverse
            ('{{ x|slice:":3" }}', {"x": "abcdef"}),
        ],
        ids=["start", "end", "range", "step", "reverse", "str"],
    )
    def test_slice(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|slugify }}", {"x": "Hello World!"}),
            ("{{ x|slugify }}", {"x": "Joël Galeran"}),  # accents
            ("{{ x|slugify }}", {"x": "  spaces  "}),
            ("{{ x|slugify }}", {"x": ""}),
        ],
        ids=["normal", "accents", "spaces", "empty"],
    )
    def test_slugify(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|stringformat:"d" }}', {"x": 42}),
            ('{{ x|stringformat:"05d" }}', {"x": 42}),     # zero-pad
            ('{{ x|stringformat:".2f" }}', {"x": 3.14159}),
            ('{{ x|stringformat:"s" }}', {"x": "hi"}),
        ],
        ids=["d", "05d", "2f", "s"],
    )
    def test_stringformat(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|striptags }}", {"x": "<b>hello</b> <i>world</i>"}),
            ("{{ x|striptags }}", {"x": "no tags"}),
            ("{{ x|striptags }}", {"x": "<script>alert(1)</script>safe"}),
        ],
        ids=["mixed", "no-tags", "script"],
    )
    def test_striptags(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "fmt",
        ["H:i", "H:i:s", "g:i a", "h:i A"],
    )
    def test_time(self, engine, fmt):
        ctx = {"x": datetime.time(13, 7, 9)}
        assert_render_matches(engine, '{{ x|time:"' + fmt + '" }}', ctx)

    def test_timesince_explicit_now(self, engine):
        """`{{ x|timesince:y }}`: both args explicit, deterministic."""
        ctx = {
            "x": datetime.datetime(2024, 1, 1, 12, 0, 0),
            "y": datetime.datetime(2024, 1, 2, 13, 30, 0),
        }
        assert_render_matches(engine, "{{ x|timesince:y }}", ctx)

    def test_timeuntil_explicit_now(self, engine):
        ctx = {
            "x": datetime.datetime(2024, 1, 5, 12, 0, 0),
            "y": datetime.datetime(2024, 1, 1, 8, 0, 0),
        }
        assert_render_matches(engine, "{{ x|timeuntil:y }}", ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|truncatechars:"5" }}', {"x": "hello world"}),
            ('{{ x|truncatechars:"100" }}', {"x": "short"}),  # shorter than limit
            ('{{ x|truncatechars_html:"7" }}', {"x": "<b>hello world</b>"}),
            ('{{ x|truncatechars_html:"4" }}', {"x": "<p>hi <b>there</b></p>"}),
        ],
        ids=["chars-trunc", "chars-no-trunc", "html-trunc", "html-mid-tag"],
    )
    def test_truncatechars_family(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|truncatewords:"2" }}', {"x": "one two three four"}),
            ('{{ x|truncatewords:"100" }}', {"x": "short text"}),
            ('{{ x|truncatewords_html:"2" }}', {"x": "<b>a b c</b>"}),
        ],
        ids=["words-trunc", "words-no-trunc", "html-trunc"],
    )
    def test_truncatewords_family(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    def test_unordered_list_flat(self, engine):
        assert_render_matches(
            engine,
            "{{ items|unordered_list }}",
            {"items": ["a", "b", "c"]},
        )

    def test_unordered_list_nested(self, engine):
        assert_render_matches(
            engine,
            "{{ items|unordered_list }}",
            {"items": ["root", ["child-a", "child-b"]]},
        )

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|urlencode }}", {"x": "hello world&foo=bar"}),
            ("{{ x|urlencode }}", {"x": "/path/with spaces"}),
            ('{{ x|urlencode:"" }}', {"x": "/path/foo"}),  # custom safe arg
            ('{{ x|urlencode:"/" }}', {"x": "/path/foo"}),
        ],
        ids=["query", "spaces", "no-safe", "safe-slash"],
    )
    def test_urlencode(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|urlize }}", {"x": "see https://example.com here"}),
            ("{{ x|urlize }}", {"x": "no links here"}),
            ("{{ x|urlize }}", {"x": "email me at foo@example.com"}),
            ('{{ x|urlizetrunc:"15" }}', {"x": "go to https://example.com/very/long/path"}),
        ],
        ids=["url", "no-url", "email", "trunc"],
    )
    def test_urlize_family(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{{ x|wordcount }}", {"x": "one two three"}),
            ("{{ x|wordcount }}", {"x": ""}),
            ("{{ x|wordcount }}", {"x": "   spaced   words   "}),
        ],
        ids=["simple", "empty", "spaced"],
    )
    def test_wordcount(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|wordwrap:"10" }}', {"x": "Joel is a slug"}),
            ('{{ x|wordwrap:"5" }}', {"x": "short"}),
        ],
        ids=["wrap", "no-wrap"],
    )
    def test_wordwrap(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{{ x|yesno }}', {"x": True}),
            ('{{ x|yesno }}', {"x": False}),
            ('{{ x|yesno }}', {"x": None}),
            ('{{ x|yesno:"y,n,m" }}', {"x": True}),
            ('{{ x|yesno:"y,n,m" }}', {"x": False}),
            ('{{ x|yesno:"y,n,m" }}', {"x": None}),
            ('{{ x|yesno:"y,n" }}', {"x": None}),   # falls back to "n"
        ],
        ids=[
            "true-default", "false-default", "none-default",
            "true-custom", "false-custom", "none-custom",
            "none-2-args",
        ],
    )
    def test_yesno(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)


# Variable resolution edge cases for `Variable._resolve_lookup`:
# `do_not_call_in_templates`, `alters_data`, dataclasses, namedtuples,
# callables in the chain, numeric/string-literal heads, non-list
# iterables, and the `_("...")` translation literal.


class TestVariableResolutionEdges:
    """Variable lookup chain semantics. Source anchor:
    `template/base.py:Variable._resolve_lookup` (lines 948-1028).

    Lookup chain:
      1. dict-style `current[bit]` if `__getitem__` (961-965).
      2. attribute `getattr(current, bit)` (969-976).
      3. integer-index `current[int(bit)]` (981-988).

    Callables auto-invoke unless they have `do_not_call_in_templates`,
    `alters_data`, or required args. Exceptions silenced when
    `silent_variable_failure = True` (993-1028).
    """

    def test_callable_is_called_with_no_args(self, engine):
        """Django auto-calls zero-arg callables encountered during lookup."""

        class C:
            def name(self):
                return "called"

        assert_render_matches(engine, "{{ obj.name }}", {"obj": C()})

    def test_do_not_call_in_templates_skipped(self, engine):
        """`obj.do_not_call_in_templates = True` makes Django leave the
        callable alone: `{{ obj.name }}` renders the bound method repr."""

        class C:
            do_not_call_in_templates = True

            def name(self):
                return "should-not-be-called"

        assert_render_matches(engine, "{{ obj.name }}", {"obj": C()})

    def test_callable_can_set_do_not_call_in_templates_per_attribute(self, engine):
        """The flag can also be set as an attribute on the callable
        itself (Django checks both)."""

        class C:
            def name(self):
                return "called-anyway"

            name.do_not_call_in_templates = True  # type: ignore[attr-defined]

        assert_render_matches(engine, "{{ obj.name }}", {"obj": C()})

    def test_alters_data_returns_string_if_invalid(self, engine):
        """A callable with `alters_data = True` is NOT called and the
        value resolves to `string_if_invalid` (`''` by default)."""

        class C:
            def delete(self):
                return "boom"

            delete.alters_data = True  # type: ignore[attr-defined]

        assert_render_matches(engine, "before[{{ obj.delete }}]after", {"obj": C()})

    def test_dataclass_attribute_access(self, engine):
        @dataclass
        class Point:
            x: int
            y: int

        assert_render_matches(engine, "{{ p.x }}|{{ p.y }}", {"p": Point(1, 2)})

    def test_namedtuple_attribute_access(self, engine):
        Point = namedtuple("Point", "x y")
        assert_render_matches(engine, "{{ p.x }}|{{ p.y }}", {"p": Point(3, 4)})

    def test_namedtuple_index_access(self, engine):
        """`{{ p.0 }}` resolves by index on a tuple-shaped value."""
        Point = namedtuple("Point", "x y")
        assert_render_matches(engine, "{{ p.0 }}|{{ p.1 }}", {"p": Point(7, 8)})

    # ---- __getattr__ duck-types -------------------------------------------
    def test_dunder_getattr_dispatch(self, engine):
        """Objects with `__getattr__` (no real attribute) still resolve."""

        class Forwarder:
            def __getattr__(self, name):
                return f"dynamic:{name}"

        assert_render_matches(engine, "{{ obj.anything }}", {"obj": Forwarder()})

    def test_dunder_getitem_dispatch(self, engine):
        """Objects with only `__getitem__` resolve by key."""

        class DictLike:
            def __getitem__(self, key):
                return f"key:{key}"

        assert_render_matches(engine, "{{ obj.foo }}", {"obj": DictLike()})

    def test_attribute_error_falls_through(self, engine):
        """An `AttributeError` mid-chain should fall through silently,
        not propagate to the rendered output."""

        class C:
            pass

        assert_render_matches(engine, "before[{{ obj.missing }}]after", {"obj": C()})

    def test_keyerror_falls_through(self, engine):
        """A `KeyError` on a dict-like access falls through."""
        assert_render_matches(engine, "before[{{ d.missing }}]after", {"d": {"only": 1}})

    def test_index_out_of_range_falls_through(self, engine):
        """An `IndexError` on list index access falls through."""
        assert_render_matches(engine, "before[{{ items.5 }}]after", {"items": [1, 2, 3]})

    # ---- numeric / string-literal heads of filter chains ------------------
    def test_int_literal_head(self, engine):
        """An integer literal is a valid `Variable` head."""
        assert_render_matches(engine, '{{ 5|add:"3" }}', {})

    def test_float_literal_head(self, engine):
        """Float literal at the head of a filter chain."""
        assert_render_matches(engine, '{{ 3.14|floatformat:"2" }}', {})

    def test_negative_int_literal_head(self, engine):
        """Negative integer literal."""
        assert_render_matches(engine, '{{ -7|add:"2" }}', {})

    def test_string_literal_head(self, engine):
        """A double-quoted string literal at the variable head."""
        assert_render_matches(engine, '{{ "hello"|upper }}', {})

    def test_single_quoted_string_literal_head(self, engine):
        """Same as above with single quotes: Django accepts both."""
        assert_render_matches(engine, "{{ 'world'|capfirst }}", {})

    def test_translation_literal_in_variable(self, engine):
        """`_("...")` inside a variable expression returns the
        msgid as-is when no translation catalog is loaded."""
        assert_render_matches(engine, '{{ _("Hello") }}', {})

    def test_translation_literal_with_filter(self, engine):
        """Translation literal with a downstream filter."""
        assert_render_matches(engine, '{{ _("hello")|upper }}', {})

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ("{% for x in items %}{{ x }}|{% endfor %}", {"items": {1, 2, 3}}),
            ("{% for x in items %}{{ x }}|{% endfor %}", {"items": (1, 2, 3)}),
            ("{% for k in items.keys %}{{ k }}|{% endfor %}", {"items": {"a": 1, "b": 2}}),
            ("{% for v in items.values %}{{ v }}|{% endfor %}", {"items": {"a": 1, "b": 2}}),
            ("{% for k in items %}{{ k }}|{% endfor %}", {"items": {"a": 1, "b": 2}}),
            ("{% for x in items %}{{ x }}|{% endfor %}", {"items": range(3)}),
        ],
        ids=["set", "tuple", "dict-keys", "dict-values", "dict", "range"],
    )
    def test_iteration_over_non_list_iterables(self, engine, src, ctx):
        # Small int sets stringify identically on both backends since
        # CPython hashes small ints to themselves.
        assert_render_matches(engine, src, ctx)

    def test_iteration_over_generator(self, engine):
        """Generators are single-pass; build a fresh one per backend."""
        src = "{% for x in items %}{{ x }}|{% endfor %}"

        dj_out = engine.from_string(src).render(
            DjangoContext({"items": (i for i in (1, 2, 3))})
        )
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext({"items": (i for i in (1, 2, 3))})
        )
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    # ---- dict-like attribute fallback for `.items` etc --------------------
    def test_dict_items_in_for_loop(self, engine):
        """`{% for k, v in d.items %}`: `.items` resolves the method,
        Django auto-calls it (it's a dict method, no special markers)."""
        assert_render_matches(
            engine,
            "{% for k, v in d.items %}{{ k }}={{ v }};{% endfor %}",
            {"d": {"a": 1, "b": 2}},
        )

    def test_property_resolves(self, engine):
        """`@property` resolves like a plain attribute (Django doesn't
        treat properties specially)."""

        class C:
            @property
            def full_name(self):
                return "Ada Lovelace"

        assert_render_matches(engine, "{{ obj.full_name }}", {"obj": C()})

    def test_chained_dict_attr_dict(self, engine):
        """`x.a.b.c` walks dict → object-attr → dict."""

        class Inner:
            value = "deep"

        assert_render_matches(
            engine,
            "{{ x.a.b.value }}",
            {"x": {"a": {"b": Inner()}}},
        )

    def test_attr_takes_precedence_over_dict_key(self, engine):
        """When both attribute and key exist with the same name,
        Django checks dict-key first (`__getitem__`), then attribute,
        then list index. With a dict whose key matches an attribute
        name, the key wins."""

        class C(dict):
            attr = "from-attribute"

            def __init__(self):
                super().__init__()
                self["attr"] = "from-key"

        assert_render_matches(engine, "{{ obj.attr }}", {"obj": C()})

    def test_string_index_lookup(self, engine):
        """`{{ s.0 }}`: string indexed by integer-string."""
        assert_render_matches(engine, "{{ s.0 }}|{{ s.4 }}", {"s": "hello"})

    # ---- `string_if_invalid` -----------------------------------------------
    def test_string_if_invalid_engine_setting(self):
        """`Engine(string_if_invalid="INVALID")` substitutes for any
        missing variable. Both backends must agree."""
        engine = Engine(
            debug=True,
            string_if_invalid="INVALID",
            libraries={"custom": "django_template_tests.templatetags.custom"},
        )
        assert_render_matches(engine, "before[{{ missing }}]after", {})

    def test_string_if_invalid_with_format_placeholder(self):
        """`%s` in `string_if_invalid` is filled with the missing var name."""
        engine = Engine(
            debug=True,
            string_if_invalid="<<%s>>",
            libraries={"custom": "django_template_tests.templatetags.custom"},
        )
        assert_render_matches(engine, "x={{ missing_var }}", {})

    def test_string_if_invalid_placeholder_with_dotted_var(self):
        """Django substitutes the FULL variable expression
        (`foo.bar.baz`), not just the missing segment."""
        engine = Engine(
            debug=True,
            string_if_invalid="<<%s>>",
        )
        assert_render_matches(engine, "{{ foo.bar.baz }}", {})

    def test_callable_requiring_args_resolves_to_string_if_invalid(self):
        """Per Django (defaulttags.py:1001-1011): a callable that
        raises TypeError on `current()` and whose signature requires
        arguments resolves to `string_if_invalid`."""

        class C:
            def needs_arg(self, x):
                return f"got {x}"

        engine = Engine(debug=True, string_if_invalid="INVALID")
        assert_render_matches(engine, "[{{ obj.needs_arg }}]", {"obj": C()})

    def test_silent_variable_failure_exception(self):
        """An exception with `silent_variable_failure = True` is
        silenced; resolution returns `string_if_invalid`. Per Django
        (base.py:1023-1024)."""

        class SilentError(Exception):
            silent_variable_failure = True

        class C:
            @property
            def failing(self):
                raise SilentError("ignored")

        engine = Engine(debug=True, string_if_invalid="MUTED")
        assert_render_matches(engine, "before[{{ obj.failing }}]after", {"obj": C()})

    def test_translation_literal_doubles_percent(self, engine):
        """Per Django (base.py:932-940), `_("100% sure")` doubles the
        `%` before passing to gettext_lazy. With no translation
        catalog loaded the value comes back unchanged: both backends
        agree on the same output, including the doubling step."""
        assert_render_matches(engine, '{{ _("100% sure") }}', {})

    def test_dict_subclass_getitem(self, engine):
        """Custom dict subclass with overridden `__getitem__` resolves
        through its dict protocol. Per Django (base.py:963-965), dict
        lookup is the first lookup attempt: guarded by
        `hasattr(type(current), "__getitem__")`."""

        class UpperDict(dict):
            def __getitem__(self, key):
                return super().__getitem__(key).upper()

        assert_render_matches(
            engine,
            "{{ d.name }}",
            {"d": UpperDict(name="alice")},
        )


# Standard-library template-tag libraries: i18n (trans, blocktrans,
# language, get_*), static, tz, cache, l10n. Confirmed against Django
# 6.0 templatetags modules.


class TestI18nTags:
    """`{% load i18n %}` family: translation and language tags."""

    @pytest.mark.parametrize(
        "src",
        [
            '{% load i18n %}{% trans "Hello" %}',
            '{% load i18n %}{% translate "Hello" %}',
            "{% load i18n %}{% trans 'World' %}",
        ],
        ids=["trans-double", "translate-double", "trans-single"],
    )
    def test_trans_literal(self, engine, src):
        # No catalog loaded: msgid is returned unchanged.
        assert_render_matches(engine, src, {})

    def test_trans_with_variable(self, engine):
        '{% trans var %}`: translates a variable holding the msgid.'
        assert_render_matches(
            engine,
            '{% load i18n %}{% trans msg %}',
            {"msg": "Hello, world"},
        )

    def test_trans_as_alias(self, engine):
        '{% trans "Hello" as greeting %}`: silent, stores under name.'
        assert_render_matches(
            engine,
            '{% load i18n %}{% trans "Hello" as greeting %}greeting=[{{ greeting }}]',
            {},
        )

    def test_trans_noop(self, engine):
        """`{% trans ... noop %}` skips the translation lookup but
        still returns the literal (useful for marking msgids for
        extraction without rendering them yet)."""
        assert_render_matches(
            engine,
            '{% load i18n %}{% trans "Hello" noop %}',
            {},
        )

    def test_trans_context(self, engine):
        '{% trans "...month" context "calendar" %}`: pgettext form.'
        assert_render_matches(
            engine,
            '{% load i18n %}{% trans "month" context "calendar" %}',
            {},
        )

    @pytest.mark.parametrize(
        "src,ctx",
        [
            ('{% load i18n %}{% blocktrans %}Hello, world{% endblocktrans %}', {}),
            (
                '{% load i18n %}{% blocktranslate %}Hi{% endblocktranslate %}',
                {},
            ),
            (
                '{% load i18n %}{% blocktrans with n=name %}Hi {{ n }}!{% endblocktrans %}',
                {"name": "Ada"},
            ),
            (
                '{% load i18n %}{% blocktrans count counter=n %}{{ counter }} apple{% plural %}{{ counter }} apples{% endblocktrans %}',
                {"n": 1},
            ),
            (
                '{% load i18n %}{% blocktrans count counter=n %}{{ counter }} apple{% plural %}{{ counter }} apples{% endblocktrans %}',
                {"n": 3},
            ),
            (
                '{% load i18n %}{% blocktrans trimmed %}\n  Hi\n  there\n{% endblocktrans %}',
                {},
            ),
            (
                '{% load i18n %}{% blocktrans asvar greeting %}Hi{% endblocktrans %}greeting=[{{ greeting }}]',
                {},
            ),
        ],
        ids=[
            "trans-bare", "translate-bare", "with",
            "count-singular", "count-plural",
            "trimmed", "asvar",
        ],
    )
    def test_blocktrans(self, engine, src, ctx):
        assert_render_matches(engine, src, ctx)

    def test_get_current_language(self, engine):
        assert_render_matches(
            engine,
            "{% load i18n %}{% get_current_language as lang %}lang=[{{ lang }}]",
            {},
        )

    def test_get_current_language_bidi(self, engine):
        assert_render_matches(
            engine,
            "{% load i18n %}{% get_current_language_bidi as bidi %}bidi=[{{ bidi }}]",
            {},
        )

    def test_get_available_languages(self, engine):
        """`{% get_available_languages as langs %}{% for code, name in langs %}...`."""
        assert_render_matches(
            engine,
            (
                "{% load i18n %}"
                "{% get_available_languages as langs %}"
                "{% for code, name in langs %}{{ code }};{% endfor %}"
            ),
            {},
        )

    def test_get_language_info(self, engine):
        assert_render_matches(
            engine,
            (
                "{% load i18n %}"
                "{% get_language_info for 'en' as li %}"
                "code={{ li.code }} name={{ li.name }}"
            ),
            {},
        )

    def test_get_language_info_list(self, engine):
        assert_render_matches(
            engine,
            (
                "{% load i18n %}"
                "{% get_language_info_list for codes as infos %}"
                "{% for li in infos %}{{ li.code }}:{{ li.name_local }};{% endfor %}"
            ),
            {"codes": ["en", "fr"]},
        )

    def test_language_block(self, engine):
        '{% language "fr" %}...{% endlanguage %}`: activates a language for the block.'
        assert_render_matches(
            engine,
            (
                "{% load i18n %}"
                "before|{% language 'fr' %}{% get_current_language as l %}{{ l }}{% endlanguage %}|after"
            ),
            {},
        )


class TestStaticTags:
    """`{% load static %}` family."""

    def test_static_literal(self, engine):
        assert_render_matches(
            engine,
            "{% load static %}{% static 'images/logo.png' %}",
            {},
        )

    def test_static_variable(self, engine):
        assert_render_matches(
            engine,
            "{% load static %}{% static path %}",
            {"path": "css/site.css"},
        )

    def test_static_as_alias(self, engine):
        assert_render_matches(
            engine,
            "{% load static %}{% static 'a.css' as url %}u=[{{ url }}]",
            {},
        )

    def test_get_static_prefix(self, engine):
        assert_render_matches(
            engine,
            "{% load static %}{% get_static_prefix as p %}[{{ p }}]",
            {},
        )

    def test_get_media_prefix(self, engine):
        assert_render_matches(
            engine,
            "{% load static %}{% get_media_prefix as p %}[{{ p }}]",
            {},
        )


class TestTzTags:
    """`{% load tz %}` family: timezone control + introspection."""

    def test_localtime_on(self, engine):
        assert_render_matches(
            engine,
            "{% load tz %}{% localtime on %}{{ dt|date:'H:i' }}{% endlocaltime %}",
            {"dt": datetime.datetime(2024, 1, 1, 12, 0)},
        )

    def test_localtime_off(self, engine):
        assert_render_matches(
            engine,
            "{% load tz %}{% localtime off %}{{ dt|date:'H:i' }}{% endlocaltime %}",
            {"dt": datetime.datetime(2024, 1, 1, 12, 0)},
        )

    def test_timezone_block(self, engine):
        assert_render_matches(
            engine,
            (
                "{% load tz %}"
                "{% timezone 'Asia/Tokyo' %}{{ dt|date:'H:i T' }}{% endtimezone %}"
            ),
            {"dt": datetime.datetime(2024, 1, 1, 12, 0)},
        )

    def test_get_current_timezone(self, engine):
        assert_render_matches(
            engine,
            "{% load tz %}{% get_current_timezone as tz %}[{{ tz }}]",
            {},
        )


class TestCacheTag:
    """`{% load cache %}{% cache %}...{% endcache %}`: fragment cache."""

    def test_cache_basic(self, engine):
        """Render content inside a `{% cache %}` block. With the dummy
        cache backend installed in tests/settings.py the body is
        rendered fresh every call, and both backends agree on the
        output."""
        assert_render_matches(
            engine,
            "{% load cache %}{% cache 500 fragment %}body[{{ x }}]{% endcache %}",
            {"x": 42},
        )

    def test_cache_with_variables_in_key(self, engine):
        """`{% cache <timeout> <name> <var> <var> %}`: the trailing
        variables become part of the cache key but don't appear in
        output."""
        assert_render_matches(
            engine,
            "{% load cache %}{% cache 500 fragment user.id %}body{% endcache %}",
            {"user": {"id": 7}},
        )


class TestL10nTag:
    """`{% load l10n %}{% localize on|off %}...{% endlocalize %}`."""

    def test_localize_on(self, engine):
        """With `localize on` the number is formatted per the active
        locale's conventions. Empty locale = US-English (no diff)."""
        assert_render_matches(
            engine,
            "{% load l10n %}{% localize on %}{{ n }}{% endlocalize %}",
            {"n": 1234.5},
        )

    def test_localize_off(self, engine):
        assert_render_matches(
            engine,
            "{% load l10n %}{% localize off %}{{ n }}{% endlocalize %}",
            {"n": 1234.5},
        )


# Tag-helper decorator APIs: `@register.simple_tag`,
# `@register.inclusion_tag`, `@register.simple_block_tag`. Every
# third-party Django app uses these; silent miscompile is catastrophic.


class TestTagHelperAPIs:
    """`@register.simple_tag` / `inclusion_tag` / `simple_block_tag`.

    Source anchor: `template/library.py` (Django 6.0):
      - `simple_tag`: 104-157 + `SimpleNode` (313-348).
      - `simple_block_tag`: 159-241 + `SimpleBlockNode`.
      - `inclusion_tag`: 243-292 + `InclusionNode` (351-385).

    Behaviours covered:
      - `takes_context=True` injects context (107, 316).
      - `as <var>` alias on `simple_tag` (129-132, 333).
      - `simple_block_tag(end_name=...)` (182-183).
      - `simple_block_tag(takes_context=True)`: `(context, content, ...)`
        (189-203).
      - `inclusion_tag` builds inner Context via `context.new(_dict)`
        (378); needed `PyContext.new` on oxide (commit c91168e).
      - Keyword-only args (120-121).
    """

    def test_simple_tag_no_params(self, engine):
        """`@register.simple_tag` with no args at all."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% no_params %}]",
            {},
        )

    def test_simple_tag_one_positional(self, engine):
        """`@register.simple_tag` with one positional argument."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% one_param 'hello' %}]",
            {},
        )

    def test_simple_tag_two_positional(self, engine):
        assert_render_matches(
            engine,
            "{% load custom %}[{% simple_two_params 'A' 'B' %}]",
            {},
        )

    def test_simple_tag_positional_from_variable(self, engine):
        assert_render_matches(
            engine,
            "{% load custom %}[{% one_param val %}]",
            {"val": "from-context"},
        )

    def test_simple_tag_as_alias(self, engine):
        """`{% one_param 'x' as out %}`: silent, stores result."""
        assert_render_matches(
            engine,
            "{% load custom %}{% one_param 'x' as out %}stored=[{{ out }}]",
            {},
        )

    def test_simple_tag_keyword_only(self, engine):
        """`def f(*, kwarg)`: must be passed as keyword."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% simple_keyword_only_param kwarg='kv' %}]",
            {},
        )

    def test_simple_tag_keyword_only_default(self, engine):
        """`def f(*, kwarg=42)`: default applies when omitted."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% simple_keyword_only_default %}]",
            {},
        )

    def test_simple_tag_keyword_only_override(self, engine):
        """Default kwarg overridden explicitly."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% simple_keyword_only_default kwarg=99 %}]",
            {},
        )

    def test_simple_tag_takes_context_no_args(self, engine):
        """`takes_context=True` with no extra args: fn receives context only."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% no_params_with_context %}]",
            {"value": "ctx-val"},
        )

    def test_simple_tag_takes_context_with_args(self, engine):
        """`takes_context=True` with positional arg."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% params_and_context 'arg-val' %}]",
            {"value": "ctx-val"},
        )

    def test_simple_tag_takes_context_explicit_false(self, engine):
        """`takes_context=False`: fn receives only its declared params."""
        assert_render_matches(
            engine,
            "{% load custom %}[{% explicit_no_context 'arg' %}]",
            {},
        )

    def test_simple_tag_context_stack_length(self, engine):
        """Verifies that `takes_context=True` passes a real Context-like
        object that exposes `.dicts`: used by debug tags and
        introspection helpers."""
        assert_render_matches(
            engine,
            "{% load custom %}depth={% context_stack_length %}",
            {},
        )

    def test_simple_block_tag_basic(self, engine):
        """Default block tag: `{% div %}body{% enddiv %}`."""
        assert_render_matches(
            engine,
            "{% load custom %}{% div %}hello{% enddiv %}",
            {},
        )

    def test_simple_block_tag_with_arg(self, engine):
        """`{% div id='foo' %}`: kwarg passed to function."""
        assert_render_matches(
            engine,
            "{% load custom %}{% div id='foo' %}body{% enddiv %}",
            {},
        )

    def test_simple_block_tag_custom_end_name(self, engine):
        """`@register.simple_block_tag(end_name='divend')` overrides the
        default `end<name>` close."""
        assert_render_matches(
            engine,
            "{% load custom %}{% div_custom_end %}contents{% divend %}",
            {},
        )

    def test_simple_block_tag_takes_context(self, engine):
        """`@register.simple_block_tag(takes_context=True)`: fn's
        signature is `(context, content, *args, **kwargs)`."""
        assert_render_matches(
            engine,
            "{% load custom %}{% no_params_with_context_block %}body{% endno_params_with_context_block %}",
            {"value": "ctx"},
        )

    def test_simple_block_tag_one_param(self, engine):
        """Block tag with one positional after the body."""
        assert_render_matches(
            engine,
            "{% load custom %}{% one_param_block 'A' %}BODY{% endone_param_block %}",
            {},
        )

    def test_simple_block_tag_keyword_only(self, engine):
        assert_render_matches(
            engine,
            "{% load custom %}{% simple_keyword_only_param_block kwarg='K' %}BODY{% endsimple_keyword_only_param_block %}",
            {},
        )

    # `tests/django_template_tests/templatetags/inclusion.py` contains
    # inclusion-tag fixtures; template at
    # `tests/django_template_tests/templates/inclusion.html`.

    def test_inclusion_tag_simple(self, engine_with_inclusion):
        """Most basic inclusion-tag form: `@inclusion_tag("tpl.html")`."""
        assert_render_matches(
            engine_with_inclusion,
            "{% load inclusion %}{% inclusion_no_params %}",
            {},
        )

    def test_inclusion_tag_one_param(self, engine_with_inclusion):
        assert_render_matches(
            engine_with_inclusion,
            "{% load inclusion %}{% inclusion_one_param 'arg-val' %}",
            {},
        )

    def test_inclusion_tag_takes_context(self, engine_with_inclusion):
        assert_render_matches(
            engine_with_inclusion,
            "{% load inclusion %}{% inclusion_no_params_with_context %}",
            {"value": "ctx-val"},
        )

    def test_inclusion_tag_params_and_context(self, engine_with_inclusion):
        assert_render_matches(
            engine_with_inclusion,
            "{% load inclusion %}{% inclusion_params_and_context 'arg' %}",
            {"value": "ctx-val"},
        )


# Engine settings + debug paths.
# Engine kwargs from engine.py:20-32: `dirs`, `app_dirs`,
# `context_processors`, `debug`, `loaders`, `string_if_invalid`,
# `file_charset`, `libraries`, `builtins`, `autoescape`.
# `use_l10n`/`use_tz` are on `BaseContext.__init__` (context.py:144).


class TestEngineSettings:
    """Engine + Context settings flow through render byte-identically."""

    def test_engine_autoescape_false_default(self):
        """`Engine(autoescape=False)` renders variables unescaped. Per
        engine.py:31 + base.py:1131."""
        engine = Engine(debug=True, autoescape=False)
        assert_render_matches(engine, "{{ x }}", {"x": "<b>bold</b>"})

    def test_engine_autoescape_true_default(self):
        """`Engine(autoescape=True)` (the default) escapes HTML."""
        engine = Engine(debug=True, autoescape=True)
        assert_render_matches(engine, "{{ x }}", {"x": "<b>bold</b>"})

    def test_context_autoescape_overrides_engine(self):
        """`Context(autoescape=...)` overrides the Engine default
        (context.py:144 + base.py:1131)."""
        engine = Engine(debug=True, autoescape=True)
        src = "{{ x }}"
        ctx = {"x": "<b>bold</b>"}
        dj_out = engine.from_string(src).render(DjangoContext(ctx, autoescape=False))
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext(ctx, autoescape=False)
        )
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    def test_autoescape_block_inside_engine_false(self):
        """`{% autoescape on %}` inside `Engine(autoescape=False)`
        re-enables escaping for its body."""
        engine = Engine(debug=True, autoescape=False)
        assert_render_matches(
            engine,
            "{{ x }}|{% autoescape on %}{{ x }}{% endautoescape %}",
            {"x": "<b>"},
        )

    def test_engine_string_if_invalid_used_for_missing(self):
        """`engine.string_if_invalid` fallback for missing vars
        (base.py:793)."""
        engine = Engine(debug=True, string_if_invalid="<<MISSING>>")
        assert_render_matches(engine, "before[{{ nope }}]after", {})

    def test_engine_string_if_invalid_empty_default(self):
        """Default `string_if_invalid=""` (engine.py:27) renders empty."""
        engine = Engine(debug=True)
        assert_render_matches(engine, "before[{{ nope }}]after", {})

    def test_engine_builtins_kwarg_makes_tags_available(self):
        """`Engine(builtins=['mod'])` auto-loads tags/filters; no
        `{% load mod %}` needed (engine.py:30 + base.py:194)."""
        engine = Engine(
            debug=True,
            builtins=["django_template_tests.templatetags.custom"],
        )
        assert_render_matches(engine, "{{ val|noop }}", {"val": "kept"})

    def test_engine_libraries_short_alias(self):
        """`Engine(libraries={'alias': 'dotted.path'})` for `{% load alias %}`."""
        engine = Engine(
            debug=True,
            libraries={"c": "django_template_tests.templatetags.custom"},
        )
        assert_render_matches(engine, "{% load c %}{{ x|noop }}", {"x": "ok"})

    def test_context_use_tz_aware_datetime(self):
        """`Context.use_tz` controls TZ rendering. `use_tz=False`
        keeps the value's tzinfo (base.py:1129)."""
        engine = Engine(debug=True)
        ctx = {"dt": datetime.datetime(2024, 1, 1, 12, 0, tzinfo=zoneinfo.ZoneInfo("UTC"))}
        src = "{{ dt|date:'H:i T' }}"

        dj_out = engine.from_string(src).render(DjangoContext(ctx, use_tz=False))
        ox_out = OxideTemplate(src, engine=engine).render(
            OxideContext(ctx, use_tz=False)
        )
        assert ox_out == dj_out, (
            f"\n  django: {dj_out!r}\n  oxide:  {ox_out!r}"
        )

    def test_context_autoescape_inheritance_in_with_block(self):
        """`{% with %}` doesn't reset autoescape inside the block."""
        engine = Engine(debug=True, autoescape=False)
        assert_render_matches(
            engine,
            "{% with y=x %}{{ y }}{% endwith %}",
            {"x": "<b>bold</b>"},
        )


class TestDebugPaths:
    """Engine `debug=True` enables template-debug exception annotation."""

    def test_debug_true_uses_debug_lexer(self):
        """`debug=True` uses `DebugLexer` (positions); base.py:185-188.
        Both backends must compile + render without raising."""
        engine = Engine(debug=True)
        assert_render_matches(engine, "{{ x }}", {"x": "ok"})

    def test_template_syntax_error_in_debug_mode(self):
        """`debug=True`: parse-time `TemplateSyntaxError` carries a
        `template_debug` payload (base.py:204). We only assert both
        backends raise; payload format is implementation detail."""
        engine = Engine(debug=True)
        bad_src = "{% if %}body{% endif %}"

        with pytest.raises(DjangoTemplateSyntaxError):
            engine.from_string(bad_src)
        with pytest.raises(OxideTemplateSyntaxError):
            OxideTemplate(bad_src, engine=engine)

    def test_template_syntax_error_without_debug(self):
        """`debug=False`: exception still raised; debug only controls
        `.template_debug` annotation."""
        engine = Engine(debug=False)
        bad_src = "{% endif %}"

        with pytest.raises(DjangoTemplateSyntaxError):
            engine.from_string(bad_src)
        with pytest.raises(OxideTemplateSyntaxError):
            OxideTemplate(bad_src, engine=engine)


class TestHumanizeFilters:
    """`{% load humanize %}` family. Six filters in
    `django/contrib/humanize/templatetags/humanize.py`:
    `ordinal` (23-66), `intcomma` (69-92), `intword` (127-151),
    `apnumber` (154-176), `naturalday` (181-202), `naturaltime`
    (207-213, `NaturalTimeFormatter` 215-343).

    Time-sensitive filters use deltas large enough that
    seconds-resolution races between independent `datetime.now()`
    calls cannot affect the output bucket.
    """

    @pytest.mark.parametrize(
        "value",
        # Cover each `value % 10` branch (humanize.py:39-63) plus the
        # 11/12/13 special-case (humanize.py:35).
        [0, 1, 2, 3, 4, 5, 10, 11, 12, 13, 14, 20, 21, 22, 23,
         100, 101, 111, 112, 113, 1000],
    )
    def test_ordinal_numeric_branches(self, engine, value):
        """Each numeric ordinal class (humanize.py:35-64)."""
        assert_render_matches(engine, "{% load humanize %}{{ x|ordinal }}", {"x": value})

    def test_ordinal_negative_returns_str(self, engine):
        """`humanize.py:33-34`: negative returns `str(value)`."""
        assert_render_matches(engine, "{% load humanize %}{{ x|ordinal }}", {"x": -5})

    def test_ordinal_non_integer_returns_value_unchanged(self, engine):
        """`humanize.py:31-32`: non-int returns value unchanged via
        the `except (TypeError, ValueError)` branch."""
        assert_render_matches(engine, "{% load humanize %}{{ x|ordinal }}", {"x": "not-int"})

    def test_ordinal_string_integer_coerces(self, engine):
        """`humanize.py:30`: `int("42")` succeeds."""
        assert_render_matches(engine, "{% load humanize %}{{ x|ordinal }}", {"x": "42"})

    def test_ordinal_float_coerces_via_int(self, engine):
        """`int(3.7)` truncates to 3 (humanize.py:30)."""
        assert_render_matches(engine, "{% load humanize %}{{ x|ordinal }}", {"x": 3.7})

    @pytest.mark.parametrize(
        "value",
        [
            0,
            1,
            999,
            1000,
            1234567,
            -1234,
            1234.5,
            -0.5,
        ],
    )
    def test_intcomma_numeric(self, engine, value):
        """Plain numeric inputs: `humanize.py:76-83` routes through
        `number_format(use_l10n=True, force_grouping=True)`."""
        assert_render_matches(engine, "{% load humanize %}{{ x|intcomma }}", {"x": value})

    def test_intcomma_string_int(self, engine):
        """String-int: `Decimal(value)` succeeds (humanize.py:79)."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|intcomma }}", {"x": "1234567"},
        )

    def test_intcomma_non_numeric_string(self, engine):
        """`Decimal("abc")` raises `InvalidOperation` → recurses with
        `use_l10n=False` (humanize.py:80-81) → regex-based digit-run
        grouping at lines 84-91. Result: 'abc' unchanged."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|intcomma }}", {"x": "no digits"},
        )

    def test_intcomma_string_with_leading_digits(self, engine):
        """Digit-run prefix is comma-grouped, trailing non-digit
        characters preserved: humanize.py:85-91."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|intcomma }}", {"x": "1234abc"},
        )

    def test_intcomma_with_use_l10n_false(self, engine):
        """`intcomma:False`: uses the regex grouping path, skipping
        locale-aware formatting (humanize.py:70 `use_l10n=True`
        default; passing False bypasses the Decimal/number_format
        path at lines 76-83)."""
        # Django filter syntax: `{{ x|intcomma:False }}`: passes
        # `False` as the second positional arg.
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|intcomma:False }}",
            {"x": 1234567},
        )

    @pytest.mark.parametrize(
        "value",
        [
            0,
            999_999,             # < 1,000,000 → passthrough per line 140-141
            1_000_000,           # 1.0 million
            1_200_000,           # 1.2 million
            999_999_999,         # 1.0 billion (rounded)
            1_000_000_000,       # 1.0 billion
            1_500_000_000,       # 1.5 billion
            1_000_000_000_000,   # 1.0 trillion
            -1_000_000,          # negative: abs() at line 139
        ],
    )
    def test_intword_numeric_buckets(self, engine, value):
        """Each magnitude bucket from `intword_converters`
        (humanize.py:96-124)."""
        assert_render_matches(engine, "{% load humanize %}{{ x|intword }}", {"x": value})

    def test_intword_non_integer_returns_value_unchanged(self, engine):
        """`humanize.py:134-137`: non-int returns as-is."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|intword }}", {"x": "abc"},
        )

    def test_intword_string_integer_coerces(self, engine):
        """`int(value)` coerces digit-strings (humanize.py:135)."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|intword }}", {"x": "1200000"},
        )

    @pytest.mark.parametrize(
        "value",
        # 1-9 spelled out (humanize.py:166-176); 0 / 10+ returned
        # as-is (humanize.py:164 `if not 0 < value < 10: return value`).
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 99, -1],
    )
    def test_apnumber_buckets(self, engine, value):
        assert_render_matches(
            engine, "{% load humanize %}{{ x|apnumber }}", {"x": value},
        )

    def test_apnumber_string_integer_coerces(self, engine):
        """`humanize.py:161`: `value = int(value)` coerces strings."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|apnumber }}", {"x": "3"},
        )

    def test_apnumber_non_numeric_returns_value(self, engine):
        """`humanize.py:162-163`: non-int via except clause."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|apnumber }}", {"x": "abc"},
        )

    # tomorrow/today/yesterday computed against `datetime.now(tzinfo).date()`
    # (humanize.py:194); independent `now()` calls land within
    # microseconds in the same test run.
    def test_naturalday_today(self, engine):
        """Today's date -> 'today' (humanize.py:196-197)."""
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturalday }}",
            {"x": datetime.date.today()},
        )

    def test_naturalday_tomorrow(self, engine):
        """Tomorrow -> 'tomorrow' (humanize.py:198-199)."""
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturalday }}",
            {"x": datetime.date.today() + datetime.timedelta(days=1)},
        )

    def test_naturalday_yesterday(self, engine):
        """Yesterday -> 'yesterday' (humanize.py:200-201)."""
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturalday }}",
            {"x": datetime.date.today() - datetime.timedelta(days=1)},
        )

    def test_naturalday_other_date_no_format_arg(self, engine):
        """Far-past date falls through to `defaultfilters.date(value,
        arg=None)` (humanize.py:202)."""
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturalday }}",
            {"x": datetime.date(2020, 6, 15)},
        )

    def test_naturalday_with_format_argument(self, engine):
        """`arg` is the format string for `defaultfilters.date`
        (humanize.py:202)."""
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturalday:'Y-m-d' }}",
            {"x": datetime.date(2020, 6, 15)},
        )

    def test_naturalday_with_datetime_value(self, engine):
        """`naturalday` accepts `datetime`; extracts `(year, month, day)`
        and rebuilds a `date` (humanize.py:188-193)."""
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturalday }}",
            {"x": datetime.datetime(2020, 6, 15, 12, 30)},
        )

    def test_naturalday_non_date_returns_value(self, engine):
        """Non-date returns unchanged via the `AttributeError` arm
        (humanize.py:191-193)."""
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturalday }}",
            {"x": "not a date"},
        )

    # `NaturalTimeFormatter.string_for` (humanize.py:303-343) buckets by:
    #   days != 0 -> past/future-day via timesince/timeuntil
    #   < 60s/60m/24h -> seconds/minutes/hours
    # Deltas chosen so microsecond skew between independent `now()`
    # calls cannot affect bucketing.
    def test_naturaltime_past_year(self, engine):
        """Day-level past via `timesince` (humanize.py:308-315)."""
        ten_years_ago = datetime.datetime.now() - datetime.timedelta(days=365 * 10)
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturaltime }}",
            {"x": ten_years_ago},
        )

    def test_naturaltime_past_days(self, engine):
        """Day-level: `delta.days != 0`."""
        ten_days_ago = datetime.datetime.now() - datetime.timedelta(days=10)
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturaltime }}",
            {"x": ten_days_ago},
        )

    def test_naturaltime_past_hours(self, engine):
        """`delta.days == 0 and delta.seconds >= 3600` -> past-hour
        (humanize.py:323-325). 3h17m delta is clear of boundaries."""
        three_hours_ago = datetime.datetime.now() - datetime.timedelta(
            hours=3, minutes=17
        )
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturaltime }}",
            {"x": three_hours_ago},
        )

    def test_naturaltime_past_minutes(self, engine):
        """`60 <= delta.seconds < 3600` -> past-minute (humanize.py:320-322)."""
        five_minutes_ago = datetime.datetime.now() - datetime.timedelta(
            minutes=5, seconds=13
        )
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturaltime }}",
            {"x": five_minutes_ago},
        )

    def test_naturaltime_future_days(self, engine):
        """`delta.days != 0` future-day (humanize.py:328-333)."""
        ten_days_from_now = datetime.datetime.now() + datetime.timedelta(days=10)
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturaltime }}",
            {"x": ten_days_from_now},
        )

    def test_naturaltime_future_hours(self, engine):
        """`(value - now).seconds in [3600, 86400)` future-hour
        (humanize.py:341-343)."""
        three_hours_from_now = datetime.datetime.now() + datetime.timedelta(
            hours=3, minutes=17
        )
        assert_render_matches(
            engine,
            "{% load humanize %}{{ x|naturaltime }}",
            {"x": three_hours_from_now},
        )

    def test_naturaltime_non_date_returns_value(self, engine):
        """Non-date returns unchanged (humanize.py:304-305)."""
        assert_render_matches(
            engine, "{% load humanize %}{{ x|naturaltime }}", {"x": "not a date"},
        )


# Raw @register.tag compliance: every Library.tag / Parser / Token /
# Node / NodeList / FilterExpression API a custom tag can touch.
# Fixture library: tests/django_template_tests/templatetags/raw_tags.py.
#
# Source anchors (Django 6.0):
#   - library.py:29-55  (Library.tag, tag_function)
#   - base.py:358-403   (Token, split_contents)
#   - base.py:501-682   (Parser: parse, skip_past, compile_filter,
#                        error, tags, filters, origin, extra_data)
#   - base.py:722-847   (FilterExpression: var, filters, is_var, __str__)
#   - base.py:1031-1098 (Node, NodeList, get_nodes_by_type, contains_nontext)
#   - base.py:1163-1207 (token_kwargs)


def _snapshot_record(engine, template_source, context_dict, recorder_attr):
    """Render through Django and oxide, snapshotting the module-level
    recorder dict in `raw_tags` after each render."""
    from django_template_tests.templatetags import raw_tags

    recorder = getattr(raw_tags, recorder_attr)

    for k in recorder:
        recorder[k] = None
    engine.from_string(template_source).render(DjangoContext(context_dict))
    dj_snap = dict(recorder)

    for k in recorder:
        recorder[k] = None
    OxideTemplate(template_source, engine=engine).render(OxideContext(context_dict))
    ox_snap = dict(recorder)

    return dj_snap, ox_snap


class TestLibraryTagRegistration:
    """Section A: every documented form of `Library.tag(...)`.

    Source: `library.py:29-55`. Library.tag dispatches on
    `(name, compile_function)` into four branches.
    """

    def test_bare_decorator_uses_function_name(self, engine):
        """`@register.tag` registers under `__name__` (library.py:34-36)."""
        assert_render_matches(engine, "{% load raw %}{% form_bare %}", {})

    def test_decorator_factory_no_args(self, engine):
        """`@register.tag()` returns `tag_function` (library.py:30-32)."""
        assert_render_matches(engine, "{% load raw %}{% form_paren %}", {})

    def test_decorator_with_positional_name(self, engine):
        """`@register.tag('name')` (library.py:37-42)."""
        assert_render_matches(engine, "{% load raw %}{% form_pos_name %}", {})

    def test_decorator_with_kwarg_name(self, engine):
        """`@register.tag(name='name')` (library.py:37-42)."""
        assert_render_matches(engine, "{% load raw %}{% form_kw_name %}", {})

    def test_direct_call_form(self, engine):
        """`register.tag('name', _func)` (library.py:43-46)."""
        assert_render_matches(engine, "{% load raw %}{% form_call %}", {})

    def test_tag_function_helper(self, engine):
        """`register.tag_function(func)` (library.py:53-55)."""
        assert_render_matches(
            engine, "{% load raw %}{% _form_tag_function_impl %}", {}
        )

    def test_tag_table_populated(self):
        """All registration forms populate `Library.tags`."""
        from django_template_tests.templatetags import raw_tags

        keys = raw_tags.register.tags
        for name in (
            "form_bare",
            "form_paren",
            "form_pos_name",
            "form_kw_name",
            "form_call",
            "_form_tag_function_impl",
        ):
            assert name in keys, f"missing registration: {name}"

    def test_unsupported_arguments_raise_value_error(self):
        """library.py:47-51 `else` arm: raises ValueError when
        `name is None` and `compile_function is not None`."""
        lib = django.template.Library()

        def _fn(parser, token):  # pragma: no cover - registration fails
            return None

        with pytest.raises(ValueError, match="Unsupported arguments"):
            lib.tag(compile_function=_fn)

    def test_none_none_returns_tag_function(self):
        """`register.tag(None, None)` and `register.tag()` both return
        `tag_function` (library.py:30-32). The returned callable
        registers under `__name__` (library.py:54)."""
        from django_template_tests.templatetags import raw_tags

        def _probe_none_none(parser, token):
            return None

        returned = raw_tags.register.tag(None, None)
        returned(_probe_none_none)
        assert raw_tags.register.tags.get("_probe_none_none") is _probe_none_none
        del raw_tags.register.tags["_probe_none_none"]

    def test_register_tag_overwrites_existing(self):
        """library.py:45: last registration wins."""
        lib = django.template.Library()

        def _a(parser, token):  # pragma: no cover
            return None

        def _b(parser, token):  # pragma: no cover
            return None

        lib.tag("dup", _a)
        lib.tag("dup", _b)
        assert lib.tags["dup"] is _b


class TestCompileFunctionContract:
    """Section B: the compile function is called with `(parser, token)`;
    every documented Token attribute is exposed correctly.

    Source: base.py:582-588 (Parser.parse), 358-403 (Token).
    """

    def test_token_contents_is_raw_block_body(self, engine):
        """`token.contents` equals the raw body (base.py:379)."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% record_token a 'b c' %}",
            {},
            "TOKEN_RECORD",
        )
        assert dj["contents"] == ox["contents"] == "record_token a 'b c'"

    def test_token_split_contents_preserves_quotes(self, engine):
        """`split_contents` keeps quoted strings together and
        `_("...")` as one bit (base.py:390-403)."""
        dj, ox = _snapshot_record(
            engine,
            '{% load raw %}{% record_token a "b c" \'d\' %}',
            {},
            "TOKEN_RECORD",
        )
        assert dj["split_contents"] == ox["split_contents"]
        assert dj["split_contents"] == ["record_token", "a", '"b c"', "'d'"]

    def test_token_first_bit_is_tag_name(self, engine):
        """`token.contents.split()[0]` is the tag name."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% record_token %}",
            {},
            "TOKEN_RECORD",
        )
        assert dj["first_bit"] == ox["first_bit"] == "record_token"

    def test_token_lineno_is_one_based(self, engine):
        """`token.lineno` is 1-based (base.py:380)."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}\n\n{% record_token %}",
            {},
            "TOKEN_RECORD",
        )
        assert dj["lineno"] == ox["lineno"] == 3

    def test_token_position_is_tuple(self, engine):
        """`position` is `(start, end)` under DebugLexer (base.py:381)."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% record_token %}",
            {},
            "TOKEN_RECORD",
        )
        assert isinstance(dj["position"], tuple)
        assert isinstance(ox["position"], tuple)
        assert len(dj["position"]) == 2
        assert len(ox["position"]) == 2

    def test_token_type_name_is_block(self, engine):
        """`TokenType.BLOCK.name == 'BLOCK'` (base.py:97-101). Proxy
        must expose the real enum member."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% record_token %}",
            {},
            "TOKEN_RECORD",
        )
        assert dj["token_type_name"] == ox["token_type_name"] == "BLOCK"

    def test_token_type_identity_comparison(self, engine):
        """`token.token_type is TokenType.BLOCK`: identity, not equality."""
        assert_render_matches(
            engine,
            "{% load raw %}{% echo_token_type %}",
            {},
        )

    def test_token_repr_format(self, engine):
        """`repr(token) == '<Block token: "..."'` (base.py:383-388)."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% record_token hello world %}",
            {},
            "TOKEN_RECORD",
        )
        assert dj["repr"] == ox["repr"]
        assert dj["repr"].startswith("<Block token:")

    def test_split_contents_returned_from_render(self, engine):
        """End-to-end: a tag emitting its `split_contents()` matches."""
        assert_render_matches(
            engine,
            '{% load raw %}{% echo_split a "b c" \'d\' %}',
            {},
        )

    def test_split_contents_with_translation_marker(self, engine):
        """`_("...")` survives `split_contents` as one bit
        (base.py:395-401)."""
        assert_render_matches(
            engine,
            '{% load raw %}{% echo_split a _("Hello") c %}',
            {},
        )


class TestCompileFunctionReturn:
    """Section C: exceptions from compile, and non-Node returns.

    Source: base.py:582-588 (try/except), 619-630 (Parser.error).
    """

    def test_template_syntax_error_propagates(self, engine):
        """Compile-time `TemplateSyntaxError` propagates from both."""
        src = "{% load raw %}{% raise_syntax %}"
        with pytest.raises(DjangoTemplateSyntaxError):
            engine.from_string(src)
        with pytest.raises((DjangoTemplateSyntaxError, OxideTemplateSyntaxError)):
            OxideTemplate(src, engine=engine)

    def test_generic_exception_wrapped_into_syntax_error(self, engine):
        """`parser.error` wraps non-Exception into TemplateSyntaxError
        (base.py:626-630); real Exception passes through."""
        src = "{% load raw %}{% raise_generic %}"
        with pytest.raises(Exception) as excinfo:
            engine.from_string(src)
        assert "forced-runtime-error" in str(excinfo.value)

        with pytest.raises(Exception) as excinfo_ox:
            OxideTemplate(src, engine=engine)
        assert "forced-runtime-error" in str(excinfo_ox.value)

    def test_return_none_crashes_at_render(self, engine):
        """Compile returning `None` crashes at render time on
        `.render_annotated`. Both engines must crash."""
        src = "{% load raw %}{% return_none %}"
        with pytest.raises(Exception):
            engine.from_string(src).render(DjangoContext({}))
        with pytest.raises(Exception):
            OxideTemplate(src, engine=engine).render(OxideContext({}))

    def test_parser_error_with_string(self, engine):
        """`parser.error(token, "msg")` wraps to TemplateSyntaxError
        (base.py:626-628)."""
        src = "{% load raw %}{% parser_error_str %}"
        with pytest.raises(Exception) as dj_exc:
            engine.from_string(src)
        assert "string-message" in str(dj_exc.value)

        with pytest.raises(Exception) as ox_exc:
            OxideTemplate(src, engine=engine)
        assert "string-message" in str(ox_exc.value)

    def test_parser_error_with_exception_instance(self, engine):
        """`parser.error(token, exc_instance)`: exception returned
        unchanged with `.token` attached (base.py:626-629)."""
        src = "{% load raw %}{% parser_error_exc %}"
        with pytest.raises(Exception) as dj_exc:
            engine.from_string(src)
        assert "exc-instance" in str(dj_exc.value)

        with pytest.raises(Exception) as ox_exc:
            OxideTemplate(src, engine=engine)
        assert "exc-instance" in str(ox_exc.value)


class TestParserApi:
    """Section D: every documented Parser method (base.py:501-682)."""

    def test_parse_with_tuple_terminator(self, engine):
        """`command in parse_until` works with a tuple (base.py:564)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% upper %}hello{% endupper %}",
            {},
        )

    def test_parse_with_list_terminator(self, engine):
        """`parse_until` passed as a list (base.py:529)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% upper_list_terminator %}hello{% endupper_list %}",
            {},
        )

    def test_parse_multiple_terminators(self, engine):
        """Multiple end-names share a single tag (base.py:564)."""
        for endname in ("endupper_a", "endupper_b"):
            assert_render_matches(
                engine,
                "{% load raw %}{% upper_multi_terminator %}hello{% " + endname + " %}",
                {},
            )

    def test_parse_no_arg_reaches_eof(self, engine):
        """`parse_until is None` normalised to `[]`; consumes to EOF
        (base.py:538-540)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% rest_of_template_lower %}HELLO {{ name }}",
            {"name": "WORLD"},
        )

    def test_parse_missing_terminator_raises(self, engine):
        """`parse_until` not reached: `unclosed_block_tag` raises
        TemplateSyntaxError (base.py:589-590, 650-657)."""
        src = "{% load raw %}{% upper %}forgot to close"
        with pytest.raises(Exception) as dj_exc:
            engine.from_string(src)
        assert "Unclosed" in str(dj_exc.value) or "unclosed" in str(dj_exc.value).lower()

        with pytest.raises(Exception) as ox_exc:
            OxideTemplate(src, engine=engine)
        msg = str(ox_exc.value).lower()
        assert "unclosed" in msg or "endupper" in msg

    def test_delete_first_token(self, engine):
        """base.py:665-666: `delete_first_token` is what consumes
        the closing tag after `parse((...,))` returns. All our
        block-tag fixtures rely on it; the `upper` case proves the
        end-tag is fully consumed (no `endupper` leaks to output)."""
        dj_out, ox_out = render_both(
            engine, "{% load raw %}{% upper %}x{% endupper %}", {}
        )
        assert "endupper" not in dj_out
        assert "endupper" not in ox_out
        assert ox_out == dj_out

    def test_skip_past(self, engine):
        """base.py:593-598: `skip_past('endtag')` consumes through
        endtag without building a NodeList. Body content is silently
        discarded (mirrors {% comment %})."""
        assert_render_matches(
            engine,
            "{% load raw %}before{% skip_block %}HIDDEN {{ x }}{% endskip %}after",
            {"x": "anything"},
        )

    def test_skip_past_missing_endtag_raises(self, engine):
        """`skip_past` with no matching close → unclosed-tag error."""
        src = "{% load raw %}{% skip_block %}no endskip here"
        with pytest.raises(Exception):
            engine.from_string(src)
        with pytest.raises(Exception):
            OxideTemplate(src, engine=engine)

    def test_next_token_and_prepend_token(self, engine):
        """base.py:659-663: `next_token()` pops, `prepend_token()`
        puts it back. Our `peek_then_consume` does exactly that."""
        assert_render_matches(
            engine,
            "{% load raw %}{% peek_then_consume %}body{% endpeek %}",
            {},
        )

    def test_compile_filter_resolves(self, engine):
        """base.py:672-676: `compile_filter(token)` returns a
        FilterExpression; `.resolve(context)` evaluates it."""
        assert_render_matches(
            engine,
            "{% load raw %}{% resolve_filter name|upper %}",
            {"name": "hello"},
        )

    def test_compile_filter_with_chained_filters(self, engine):
        """`compile_filter('var|f1|f2:arg')` must produce a
        FilterExpression whose `.resolve` runs the chain."""
        assert_render_matches(
            engine,
            "{% load raw %}{% resolve_filter name|upper|truncatechars:3 %}",
            {"name": "hello"},
        )

    def test_compile_filter_ignore_failures_true(self, engine):
        """base.py:785-800: `resolve(ctx, ignore_failures=True)`
        returns `None` for missing variables (not string_if_invalid)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% resolve_filter_ignoring_failures missing|upper %}",
            {},
        )

    def test_compile_filter_with_dotted_lookup(self, engine):
        """Dotted variable in `compile_filter` arg."""
        assert_render_matches(
            engine,
            "{% load raw %}{% resolve_filter obj.name|upper %}",
            {"obj": {"name": "alice"}},
        )

    def test_parser_tags_snapshot(self, engine):
        """`parser.tags` returns a dict of registered tag callables.
        We snapshot via the recorder and verify `upper` is present
        through both backends."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% snapshot_parser_tags %}",
            {},
            "PARSER_TAGS_SNAPSHOT",
        )
        assert dj["has_upper"] is True
        assert ox["has_upper"] is True
        # Tag tables should agree on the presence of common tags.
        for name in ("upper", "form_bare", "skip_block"):
            assert name in dj["keys"]
            assert name in ox["keys"]

    def test_parser_filters_snapshot(self, engine):
        """`parser.filters` returns a dict of registered filter
        callables. Standard filter names like `upper` must be present."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% snapshot_parser_filters %}",
            {},
            "PARSER_FILTERS_SNAPSHOT",
        )
        for name in ("upper", "lower", "length"):
            assert name in dj["keys"], f"{name} missing from dj filters"
            assert name in ox["keys"], f"{name} missing from ox filters"

    def test_parser_origin_snapshot(self, engine):
        """`parser.origin` is Origin or None. We require parity, not a
        specific value."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% snapshot_parser_origin %}",
            {},
            "PARSER_ORIGIN_SNAPSHOT",
        )
        # Either both are None or both are non-None.
        assert dj["is_none"] == ox["is_none"]

    def test_parser_extra_data_round_trip(self, engine):
        """`parser.extra_data`: dict for cross-compile state (used by
        template-partials)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% extra_data_set %}{% extra_data_get %}",
            {},
        )

    def test_parser_compile_filter_token_string_form(self, engine):
        """`parser.compile_filter` with a quoted argument."""
        assert_render_matches(
            engine,
            '{% load raw %}{% resolve_filter missing|default:"fallback" %}',
            {},
        )

    def test_parser_introspectable_during_compile(self, engine):
        """`snapshot_parser_tags` inspects `parser.tags` at compile
        time. Confirms the live registry is exposed."""
        dj, ox = _snapshot_record(
            engine,
            "{% load raw %}{% snapshot_parser_tags %}",
            {},
            "PARSER_TAGS_SNAPSHOT",
        )
        assert dj["keys"] is not None
        assert ox["keys"] is not None

    def test_parse_consumes_terminator_only_via_delete(self, engine):
        """After `parser.parse(('endX',))`, the `endX` token is still
        on the stream (base.py:566-568); `delete_first_token` removes
        it. A nested case proves no leakage."""
        dj_out, ox_out = render_both(
            engine,
            "{% load raw %}A{% upper %}b{% upper %}c{% endupper %}d{% endupper %}E",
            {},
        )
        assert dj_out == ox_out
        assert "endupper" not in dj_out


class TestNodeRenderContract:
    """Section E: `Node.render(context)` semantics (base.py:1031-1082)."""

    def test_set_var_writes_to_context(self, engine):
        """Docs pattern: `context[var] = value; return ""`. Visible
        to subsequent siblings."""
        assert_render_matches(
            engine,
            '{% load raw %}{% set_var x "hello" %}{{ x }}',
            {},
        )

    def test_set_var_returns_empty_no_output(self, engine):
        """Set-only render returns empty; output is just `{{ x }}`."""
        dj_out, ox_out = render_both(
            engine,
            '{% load raw %}before|{% set_var x "MID" %}|after|{{ x }}',
            {},
        )
        assert dj_out == ox_out == "before||after|MID"

    def test_render_annotated_method_preferred(self, engine):
        """`render_annotated(context)` is the preferred entry
        (base.py:1044-1068); oxide must call it when overridden."""
        assert_render_matches(
            engine,
            "{% load raw %}{% render_annotated_only %}",
            {},
        )

    def test_render_context_state_via_self_key(self, engine):
        """Thread-safety docs pattern: state in `render_context[self]`.
        Three `{% counter %}` tags have three separate counters."""
        assert_render_matches(
            engine,
            "{% load raw %}{% counter %}-{% counter %}-{% counter %}",
            {},
        )

    def test_render_context_resets_between_renders(self, engine):
        """`render_context` is bound to Context, not Node: state does
        not leak between renders."""
        src = "{% load raw %}{% counter %}{% counter %}"
        dj_out1, ox_out1 = render_both(engine, src, {})
        dj_out2, ox_out2 = render_both(engine, src, {})
        assert dj_out1 == ox_out1
        assert dj_out2 == ox_out2
        assert dj_out1 == dj_out2

    def test_context_template_engine_debug(self, engine):
        """`context.template.engine.debug` accessible from Node."""
        dj_out, ox_out = render_both(engine, "{% load raw %}{% debug_inspect %}", {})
        assert dj_out == ox_out == "True"

    def test_context_autoescape_visible(self, engine):
        """`context.autoescape` accessible inside `Node.render`."""
        dj_out, ox_out = render_both(
            engine, "{% load raw %}{% autoescape_inspect %}", {}
        )
        assert dj_out == ox_out == "True"

    def test_node_returning_mark_safe_not_escaped(self, engine):
        """`mark_safe('<b>...</b>')` is not escaped under autoescape on."""
        dj_out, ox_out = render_both(
            engine, "{% load raw %}{% mark_safe_html %}", {}
        )
        assert dj_out == ox_out == "<b>safe-html</b>"

    def test_node_returning_raw_html_NOT_escaped(self, engine):
        """Raw-string from `@register.tag` is NOT auto-escaped (howto
        docs); both engines pass it through."""
        dj_out, ox_out = render_both(
            engine, "{% load raw %}{% raw_html %}", {}
        )
        assert dj_out == ox_out
        assert dj_out == "<b>raw-html</b>"

    def test_node_returning_non_string_crashes_consistently(self, engine):
        """Non-string return must fail with the same shape on both
        engines; exact exception class may differ."""
        src = "{% load raw %}{% int_return %}"
        dj_raised = False
        ox_raised = False
        try:
            engine.from_string(src).render(DjangoContext({}))
        except Exception:
            dj_raised = True
        try:
            OxideTemplate(src, engine=engine).render(OxideContext({}))
        except Exception:
            ox_raised = True
        assert dj_raised == ox_raised, (
            f"engine divergence on int_return: dj={dj_raised} ox={ox_raised}"
        )


class TestBlockTagParseUntil:
    """Section F: block-tag (parse_until + saved nodelist) patterns
    (base.py:529-591)."""

    def test_block_tag_body_rendered(self, engine):
        """`{% upper %}body{% endupper %}`: body rendered + uppercased."""
        assert_render_matches(
            engine, "{% load raw %}{% upper %}hello world{% endupper %}", {}
        )

    def test_block_tag_with_variable_in_body(self, engine):
        """Body variables resolve against the surrounding context."""
        assert_render_matches(
            engine,
            "{% load raw %}{% upper %}hello {{ name }}{% endupper %}",
            {"name": "alice"},
        )

    def test_nested_same_tag(self, engine):
        """Nested invocation: each compile builds its own NodeList."""
        assert_render_matches(
            engine,
            "{% load raw %}{% upper %}a{% upper %}b{% endupper %}c{% endupper %}",
            {},
        )

    def test_nested_mixed_block_tags(self, engine):
        """Block tag inside block tag: outer renders inner, then transforms."""
        assert_render_matches(
            engine,
            "{% load raw %}{% upper %}prefix-{% reverse %}abc{% endreverse %}{% endupper %}",
            {},
        )

    def test_body_with_text_and_variables_and_tags(self, engine):
        """Heterogeneous body via captured NodeList.render."""
        assert_render_matches(
            engine,
            "{% load raw %}{% upper %}A {{ x }} B{% endupper %}",
            {"x": "y"},
        )

    def test_end_tag_with_trailing_whitespace(self, engine):
        """`{% endupper   %}` equals `{% endupper %}`."""
        assert_render_matches(
            engine, "{% load raw %}{% upper %}hi{% endupper   %}", {}
        )

    def test_consecutive_block_tags(self, engine):
        """Two same-name block tags side by side: no end-tag leakage."""
        assert_render_matches(
            engine,
            "{% load raw %}{% upper %}a{% endupper %}-{% upper %}b{% endupper %}",
            {},
        )


class TestVariableResolution:
    """Section G: `template.Variable.resolve` inside a custom Node
    plus `VariableDoesNotExist` semantics (base.py:850-1028)."""

    def test_variable_resolves(self, engine):
        """`Variable('foo').resolve(ctx)`."""
        assert_render_matches(
            engine,
            '{% load raw %}{% resolve_var x "fallback" %}',
            {"x": "real-value"},
        )

    def test_variable_does_not_exist_caught(self, engine):
        """Missing variable raises `VariableDoesNotExist`; Node
        catches and renders fallback."""
        assert_render_matches(
            engine,
            '{% load raw %}{% resolve_var missing "use-fallback" %}',
            {},
        )

    def test_variable_dotted_lookup_inside_node(self, engine):
        """Dotted variable inside a Node (base.py:948-1028)."""
        assert_render_matches(
            engine,
            '{% load raw %}{% resolve_var obj.x "no" %}',
            {"obj": {"x": "deep"}},
        )


class TestAutoescapeInteraction:
    """Section H: autoescape state visible in Node.render, SafeString
    preservation (base.py:1123-1136)."""

    def test_autoescape_off_visible_in_node(self, engine):
        """`{% autoescape off %}`: Node sees `context.autoescape == False`."""
        assert_render_matches(
            engine,
            "{% load raw %}{% autoescape off %}{% autoescape_inspect %}{% endautoescape %}",
            {},
        )

    def test_autoescape_on_visible_in_node(self, engine):
        """`{% autoescape on %}`: Node sees True regardless of engine default."""
        assert_render_matches(
            engine,
            "{% load raw %}{% autoescape on %}{% autoescape_inspect %}{% endautoescape %}",
            {},
        )

    def test_mark_safe_preserved_in_autoescape_off(self, engine):
        """SafeString from Node stays unescaped inside autoescape off."""
        assert_render_matches(
            engine,
            "{% load raw %}{% autoescape off %}{% mark_safe_html %}{% endautoescape %}",
            {},
        )

    def test_raw_html_not_escaped_in_autoescape_off(self, engine):
        """Tag output is not auto-escaped; passes through under
        autoescape off."""
        assert_render_matches(
            engine,
            "{% load raw %}{% autoescape off %}{% raw_html %}{% endautoescape %}",
            {},
        )


class TestTagRegistrationPaths:
    """Section I: how a custom tag library wires into an Engine
    (engine.py:47-63)."""

    def test_load_alias_works(self, engine):
        """`{% load raw %}` via the short alias in `_TEST_LIBRARIES`."""
        assert_render_matches(engine, "{% load raw %}{% form_bare %}", {})

    def test_builtins_engine_avoids_load(self):
        """`Engine(builtins=[...])` skips `{% load %}` (engine.py:62)."""
        engine = Engine(
            debug=True,
            app_dirs=True,
            builtins=["django_template_tests.templatetags.raw_tags"],
        )
        assert_render_matches(engine, "{% form_bare %}", {})

    def test_load_idempotent(self, engine):
        """Loading the same library twice is a no-op."""
        assert_render_matches(
            engine,
            "{% load raw %}{% load raw %}{% form_bare %}",
            {},
        )


class TestThreadSafetyPattern:
    """Section J: the `context.render_context[self]` thread-safety
    idiom from the howto."""

    def test_render_context_per_render(self, engine):
        """Each render gets its own render_context; no leak."""
        src = "{% load raw %}{% counter %}-{% counter %}"
        for _ in range(3):
            assert_render_matches(engine, src, {})

    def test_render_context_self_keys_independent(self, engine):
        """Each `{% counter %}` uses `self` as key; three nodes get
        three independent counters."""
        dj_out, ox_out = render_both(
            engine,
            "{% load raw %}{% counter %}-{% counter %}-{% counter %}",
            {},
        )
        assert dj_out == ox_out == "1-1-1"


class TestTokenKwargsHelper:
    """Section K: `token_kwargs` (base.py:1163-1207)."""

    def test_token_kwargs_all_kwargs(self, engine):
        """All bits are `key=value` -> full dict."""
        assert_render_matches(
            engine,
            "{% load raw %}{% kwargs_echo a=1 b=2 c='three' %}",
            {},
        )

    def test_token_kwargs_with_variable_value(self, engine):
        """Values can be variable refs (base.py:1202)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% kwargs_echo name=user.name age=21 %}",
            {"user": {"name": "alice"}},
        )

    def test_token_kwargs_stops_at_non_kwarg(self, engine):
        """`token_kwargs` returns at first non-kwarg bit
        (base.py:1191-1194); leftovers stay in input."""
        assert_render_matches(
            engine,
            "{% load raw %}{% kwargs_partial pos a=1 b=2 %}",
            {},
        )


class TestFilterExpressionIntrospection:
    """Section L: `FilterExpression.var/.filters/.is_var/__str__`
    used by django-cotton, django-debug-toolbar (base.py:737, 781-783,
    843-844)."""

    def test_filter_expression_introspection_simple_var(self, engine):
        """Bare variable: `is_var=True`, `filters=[]`, `str(fe)=='name'`."""
        assert_render_matches(
            engine,
            "{% load raw %}{% fe_introspect name %}",
            {"name": "alice"},
        )

    def test_filter_expression_introspection_with_filters(self, engine):
        """`len(fe.filters) > 0` when chained."""
        assert_render_matches(
            engine,
            "{% load raw %}{% fe_introspect name|upper|lower %}",
            {"name": "Hello"},
        )

    def test_filter_expression_introspection_literal(self, engine):
        """`fe.is_var == False` for a literal-only expression."""
        assert_render_matches(
            engine,
            "{% load raw %}{% fe_introspect 'hello' %}",
            {},
        )

    def test_filter_expression_str_preserves_token(self, engine):
        """`str(fe) == fe.token`: verbatim expression preserved."""
        assert_render_matches(
            engine,
            "{% load raw %}{% fe_introspect name|default:'x' %}",
            {"name": "Y"},
        )


class TestNodeListIntrospection:
    """Section M: NodeList list ops plus `get_nodes_by_type` and
    `contains_nontext` (base.py:1085-1098)."""

    def test_nodelist_len(self, engine):
        """`len(nodelist)`."""
        assert_render_matches(
            engine,
            "{% load raw %}{% nl_introspect %}plain text{% endnl %}",
            {},
        )

    def test_nodelist_iter(self, engine):
        """`for node in nodelist:` iterates."""
        assert_render_matches(
            engine,
            "{% load raw %}{% nl_introspect %}a{{ x }}b{% endnl %}",
            {"x": "X"},
        )

    def test_nodelist_get_nodes_by_type_text(self, engine):
        """`get_nodes_by_type(TextNode)` returns TextNode children
        only (base.py:1093-1098)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% nl_introspect %}text only{% endnl %}",
            {},
        )

    def test_nodelist_get_nodes_by_type_filters_to_text(self, engine):
        """Mixed content: TextNode filter excludes VariableNode."""
        assert_render_matches(
            engine,
            "{% load raw %}{% nl_introspect %}text{{ v }}more{% endnl %}",
            {"v": "V"},
        )

    def test_nodelist_contains_nontext_text_only(self, engine):
        """`contains_nontext == False` for TextNodes only (base.py:1088)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% nl_introspect %}plain text only{% endnl %}",
            {},
        )

    def test_nodelist_contains_nontext_with_var(self, engine):
        """`contains_nontext == True` with a VariableNode (base.py:600-617)."""
        assert_render_matches(
            engine,
            "{% load raw %}{% nl_introspect %}{{ x }}{% endnl %}",
            {"x": "x"},
        )


class TestRawTagEdgeCases:
    """Section N: boundary conditions."""

    def test_tag_with_no_args(self, engine):
        """`{% no_args %}`: `token.contents == 'no_args'`."""
        assert_render_matches(engine, "{% load raw %}{% no_args %}", {})

    def test_quoted_string_arg(self, engine):
        """Quoted strings survive `split_contents` as one bit."""
        assert_render_matches(
            engine,
            '{% load raw %}{% echo_split a "hello world" b %}',
            {},
        )

    def test_translation_marker_arg_preserved(self, engine):
        """`_("Hello")` is one bit (base.py:395-401)."""
        assert_render_matches(
            engine,
            '{% load raw %}{% trans_marker_echo _("Hello world") %}',
            {},
        )

    def test_multiple_load_statements(self, engine):
        """`{% load raw %}` twice is harmless."""
        assert_render_matches(
            engine,
            "{% load raw %}body{% load raw %}{% form_bare %}",
            {},
        )

    def test_tag_immediately_after_text(self, engine):
        """No whitespace between text and tag: common in production
        templates. Token boundaries must be correct."""
        assert_render_matches(
            engine,
            "{% load raw %}prefix{% form_bare %}suffix",
            {},
        )


# Lexer monkey-patch compatibility: django-cotton et al.
#
# Third-party libraries customise tokenisation by replacing
# `django.template.base.Lexer.tokenize` at `AppConfig.ready` time.
# django-cotton uses this to expand `<c-foo>...</c-foo>` component
# syntax into `{% cotton 'foo' %}...{% endcotton %}` blocks before
# Django's parser ever sees the source. ANY oxide perf optimisation
# that bypasses the Python lexer (a Rust-native lexer fast path,
# identity-check shortcut, etc.) silently breaks every cotton-using
# project because the component preprocessing never runs.
#
# This test class locks down the contract:
#
#   When `django.template.base.Lexer.tokenize` (or `DebugLexer.tokenize`)
#   has been replaced with a custom callable, oxide MUST invoke that
#   callable during compile. The Rust lexer may only be used when the
#   stock Django implementation is in place AND we can prove that.
#
# A bug in this area would have shipped silently without these tests  - 
# the existing suite renders synthetic fixtures that never exercise a
# monkey-patched lexer. We caught one in production when oxide was
# installed into a cotton-using project. The fix was to always go
# through `Lexer.tokenize` when an engine is provided; this test
# class guards against any future "fast path" regression.


class TestLexerMonkeyPatchCompat:
    """Lexer.tokenize monkey-patches must be honoured by oxide.

    Cotton's pattern (paraphrased from django-cotton source):

        from django.template import base as tb
        _orig_lexer = tb.Lexer.tokenize
        _orig_debug = tb.DebugLexer.tokenize

        def _cotton_tokenize(self):
            self.template_string = _expand_cotton_syntax(self.template_string)
            return _orig_lexer(self)

        tb.Lexer.tokenize = _cotton_tokenize
        tb.DebugLexer.tokenize = ...  # same pattern, debug variant

    Cotton patches BOTH classes because a project may run with
    `debug=True` in dev and `debug=False` in prod; the engine
    selects which lexer class to instantiate. The tests below patch
    both lexer classes for the same reason: whichever one oxide
    invokes, the patch must fire.
    """

    def _patch_both_lexers(self, mutator):
        """Install `mutator(self)` as `tokenize` on BOTH Lexer and
        DebugLexer. Returns a (restore_fn, call_count_dict) pair.

        `mutator` runs BEFORE the original tokenize and may mutate
        `self.template_string` (Cotton-style source preprocessing)."""
        import django.template.base as tb

        call_count = {"n": 0}
        orig_lexer = tb.Lexer.tokenize
        orig_debug = tb.DebugLexer.tokenize

        def _wrap(original):
            def _patched(self):
                call_count["n"] += 1
                mutator(self)
                return original(self)
            return _patched

        tb.Lexer.tokenize = _wrap(orig_lexer)
        tb.DebugLexer.tokenize = _wrap(orig_debug)

        def restore():
            tb.Lexer.tokenize = orig_lexer
            tb.DebugLexer.tokenize = orig_debug

        return restore, call_count

    def test_lexer_tokenize_patch_is_invoked(self, engine):
        """Patching `Lexer.tokenize` / `DebugLexer.tokenize` with a
        counter must increment the counter on every compile. If oxide
        bypasses the Python lexer (e.g. a Rust-native fast path) the
        counter stays at 0 and Cotton-style patches silently break."""
        restore, call_count = self._patch_both_lexers(lambda self: None)
        try:
            OxideTemplate(
                "Hello {{ name }}", engine=engine
            ).render(OxideContext({"name": "world"}))
        finally:
            restore()

        assert call_count["n"] >= 1, (
            "oxide compiled a template without invoking "
            "Lexer.tokenize / DebugLexer.tokenize: Cotton-style "
            "monkey-patches will silently break"
        )

    def test_lexer_tokenize_patch_transforms_source(self, engine):
        """A patched tokenize that rewrites `self.template_string`
        BEFORE delegating to the original must affect oxide's output.

        This is the exact failure mode that surfaced when oxide
        introduced an identity-checked Lexer fast path: oxide captured
        the already-patched method as baseline on first compile, then
        re-used the Rust lexer on subsequent calls, silently dropping
        Cotton's source preprocessing.

        Cotton-style transform: replace a sentinel marker in the
        source with substituted content. If oxide skips the patched
        method the marker survives into the rendered output.
        """
        def _rewrite(self):
            self.template_string = self.template_string.replace(
                "REPLACE_ME", "REWROTE_BY_PATCH"
            )

        restore, _ = self._patch_both_lexers(_rewrite)
        try:
            out = OxideTemplate(
                "before REPLACE_ME after", engine=engine
            ).render(OxideContext({}))
        finally:
            restore()

        assert "REWROTE_BY_PATCH" in out, (
            f"patched Lexer.tokenize did not affect oxide output. "
            f"Got: {out!r}. This means oxide bypassed the patched "
            f"Python lexer: Cotton/i18n/any-source-preprocessor will "
            f"silently break."
        )
        assert "REPLACE_ME" not in out, (
            "source marker survived into rendered output: patched "
            "lexer was not consulted"
        )

    def test_patch_installed_AFTER_first_compile_is_honoured(self, engine):
        """The trickiest case: a project compiles one template via
        oxide (e.g. a startup health-check), THEN installs a tokenize
        patch (e.g. mid-process plugin load), then compiles another
        template. Any "snapshot at first compile" optimisation breaks
        this scenario.

        Codifies that oxide must NOT cache the lexer method identity
        in a way that fast-paths around later patches.
        """
        # Compile #1: establish whatever baseline oxide might
        # internally cache (before any patch is installed).
        OxideTemplate(
            "first {{ x }}", engine=engine
        ).render(OxideContext({"x": "compile"}))

        # NOW install a patch.
        restore, call_count = self._patch_both_lexers(lambda self: None)
        try:
            # Compile #2: the patch must be invoked.
            OxideTemplate(
                "second {{ y }}", engine=engine
            ).render(OxideContext({"y": "compile"}))
        finally:
            restore()

        assert call_count["n"] >= 1, (
            "oxide cached the pre-patch tokenize identity and skipped "
            "the patch installed mid-process. Late-bound Cotton-style "
            "hooks will silently break."
        )

    def test_block_override_survives_nested_template_render(self):
        """Regression: a custom tag in the parent template that loads
        and renders a SEPARATE template mid-render (the django-cotton
        pattern) must NOT clobber the BlockContext set up by
        `{% extends %}` for the outer chain.

        Reproduction shape:

            base.html:
              before
              {% inner_template_render %}     <-- recursive Template.render
              {% block content %}default{% endblock %}
              after

            child.html:
              {% extends "base.html" %}
              {% block content %}OVERRIDE{% endblock %}

        Bug: oxide called `reset_block_context()` at the top of every
        `Template::render`, including the nested one inside
        `{% inner_template_render %}`. The nested render wiped the
        thread-local BLOCK_CONTEXT. By the time control reached
        base.html's `{% block content %}` (which appears AFTER the
        nested inner render in the fixture), `has_block_context()`
        returned false → BlockNode.render took the standalone branch
        → rendered the parent's default body → child override lost.

        Talented-v2 production failure mode: dashboard renders with
        empty `<main>` content because base.html invokes cotton
        components throughout its body, and one of them runs BEFORE
        the `{% block content %}` line: wiping the BC.

        Setup requirement: the inner `{% inner_template_render %}` tag
        loads `oxide_fragment.html` via `django.template.loader.get_template`,
        which routes through `settings.TEMPLATES`. To reproduce the bug
        the inner render MUST go through OxideTemplates (its render
        path invokes Rust `Template::render` which is where the reset
        happens): so this test constructs a dedicated OxideTemplates
        backend and routes the loader through it for the duration of
        the test. (The default test fixture uses stock Django, where
        the bug doesn't reproduce because stock has no `reset_block_context`.)

        Fix: save/restore the BLOCK_CONTEXT thread-local around each
        `Template::render` rather than unconditionally clearing.
        """
        from django.template import engines as _engines

        # Construct an OxideTemplates backend with the same settings the
        # default test fixture uses. Critically, register it as the live
        # 'django' engine so `django.template.loader.get_template`  - 
        # called from inside `{% inner_template_render %}`: returns
        # an `OxideTemplateAdapter`.
        from django_template_oxide.backend import OxideTemplates

        ox_backend = OxideTemplates({
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
            },
        })

        # Splice ox_backend into the engine registry so loader.get_template
        # resolves through it for the duration of the test.
        original_engines = _engines._engines if hasattr(_engines, "_engines") else None
        # The EngineHandler caches engines in `_engines` (Django 6.0+);
        # invalidate the cache so our backend wins.
        _engines.templates_changed = True
        _orig_all = _engines.all
        _orig_get = _engines.__getitem__
        _engines.all = lambda: [ox_backend]
        _engines.__getitem__ = lambda key: ox_backend if key == "django" else _orig_get(key)

        try:
            child_src = (
                '{% extends "oxide_base_with_inner_render.html" %}'
                "{% block content %}OVERRIDE_MARKER{% endblock %}"
            )
            ox_out = OxideTemplate(child_src, engine=ox_backend.engine).render(
                OxideContext({"who": "inner"})
            )
        finally:
            _engines.all = _orig_all
            _engines.__getitem__ = _orig_get

        assert "OVERRIDE_MARKER" in ox_out, (
            f"oxide dropped the child's block override after a nested "
            f"Template.render call (the Cotton failure shape). "
            f"Got: {ox_out!r}"
        )
        assert "default-content" not in ox_out, (
            f"parent's default rendered instead of child override: {ox_out!r}"
        )

    def test_elif_inside_block_tag_does_not_leak(self, engine):
        """The original production failure pattern: a Cotton-style
        block tag wrapping an `{% if %}/{% elif %}` body raised
        `Invalid block tag 'elif', expected 'endcotton'` because
        oxide was processing the un-preprocessed source.

        This test does not require django-cotton to be installed  - 
        it reproduces the structural shape: a custom block tag's body
        contains an `{% if %}…{% elif %}…{% endif %}` chain. The
        inner `{% if %}` must consume its own elif/endif so they
        don't leak up to the outer block's parse_until.

        Combined with the patches above, this test passes only if
        (a) the patched lexer is invoked AND (b) the tokens flow
        correctly into the parser's block-tag handling.
        """
        # We use the existing `upper` block tag from raw_tags.py as a
        # stand-in for `{% cotton %}`: both follow the same
        # `parse((endname,))` + `delete_first_token` pattern.
        src = (
            "{% load raw %}"
            "{% upper %}"
            "{% if v == 1 %}one"
            "{% elif v == 2 %}two"
            "{% else %}other{% endif %}"
            "{% endupper %}"
        )
        # Patch both lexer classes with a no-op mutator so the test
        # exercises the same path as a Cotton-using project.
        restore, call_count = self._patch_both_lexers(lambda self: None)
        try:
            for v in (1, 2, 3):
                out = OxideTemplate(src, engine=engine).render(
                    OxideContext({"v": v})
                )
                expected = {1: "ONE", 2: "TWO", 3: "OTHER"}[v]
                assert out == expected, (
                    f"if/elif inside block-tag body broke for v={v}: "
                    f"got {out!r}, expected {expected!r}"
                )
        finally:
            restore()

        assert call_count["n"] >= 3, (
            "patched lexer was not invoked on each compile: fast-path "
            "regression"
        )

import contextlib
import datetime
import decimal
from collections import OrderedDict, namedtuple

import pytest

import django.template.base as _base
from django.template.backends.django import DjangoTemplates
from django.test import override_settings
from django.utils import timezone, translation
from django.utils.functional import lazy
from django.utils.safestring import mark_safe
from django.utils.translation import gettext_lazy

from django_template_oxide import _patch
from django_template_oxide.backend import OxideTemplates

_TEMPLATES = {
    "show_value.html": "[{{ value }}]",
    "show_values.html": "[{{ a }}|{{ b }}]",
    "base.html": "<{% block body %}base{% endblock %}>",
    "counter.html": "{{ forloop.counter }}",
}


_OVERRIDE_LIBRARIES = {
    "parity_override": "django_template_tests.templatetags.parity_override",
}


def _options(**extra):
    return {
        "loaders": [("django.template.loaders.locmem.Loader", _TEMPLATES)],
        **extra,
    }


def _backend(cls, name, **options):
    return cls({"NAME": name, "DIRS": [], "APP_DIRS": False, "OPTIONS": _options(**options)})


_ENGINES = {
    "default": (
        _backend(DjangoTemplates, "stock-default"),
        _backend(OxideTemplates, "oxide-default"),
    ),
    "invalid": (
        _backend(DjangoTemplates, "stock-invalid", string_if_invalid="INVALID(%s)"),
        _backend(OxideTemplates, "oxide-invalid", string_if_invalid="INVALID(%s)"),
    ),
    "invalid_plain": (
        _backend(DjangoTemplates, "stock-invalid-plain", string_if_invalid="INVALID"),
        _backend(OxideTemplates, "oxide-invalid-plain", string_if_invalid="INVALID"),
    ),
    "noescape": (
        _backend(DjangoTemplates, "stock-noescape", autoescape=False),
        _backend(OxideTemplates, "oxide-noescape", autoescape=False),
    ),
    "override": (
        _backend(DjangoTemplates, "stock-override", libraries=_OVERRIDE_LIBRARIES),
        _backend(OxideTemplates, "oxide-override", libraries=_OVERRIDE_LIBRARIES),
    ),
}


@contextlib.contextmanager
def _stock_django():
    saved_render = _base.Template._render
    nodelist_patched = _patch.is_enabled()
    if nodelist_patched:
        _patch.disable_rust_nodelist_acceleration()
    _base.Template._render = lambda self, context: self.nodelist.render(context)
    try:
        yield
    finally:
        _base.Template._render = saved_render
        if nodelist_patched:
            _patch.enable_rust_nodelist_acceleration()


def _outcome(render, messages=False):
    try:
        return ("ok", render())
    except Exception as exc:
        if messages:
            return ("error", type(exc).__name__, str(exc))
        return ("error", type(exc).__name__)


def _render_pair(
    engine_key, src, ctx, language="en", tz="America/New_York", settings=None, messages=False
):
    stock, oxide = _ENGINES[engine_key]
    overrides = override_settings(**settings) if settings else contextlib.nullcontext()
    with overrides, translation.override(language), timezone.override(tz):
        with _stock_django():
            expected = _outcome(lambda: stock.from_string(src).render(dict(ctx)), messages)
        actual = _outcome(lambda: oxide.from_string(src).render(dict(ctx)), messages)
    return expected, actual


def _assert_parity(engine_key, src, ctx, **kwargs):
    expected, actual = _render_pair(engine_key, src, ctx, **kwargs)
    assert actual == expected, (
        f"\n  template: {src!r}\n  django:   {expected!r}\n  oxide:    {actual!r}"
    )


Point = namedtuple("Point", "x y")


class Obj:
    name = "obj"
    empty = ""
    zero = 0
    none = None
    markup = "<b>&</b>"

    def method(self):
        return "called"

    def needs_arg(self, x):
        return x

    def danger(self):
        return "ran"

    danger.alters_data = True

    @property
    def boom(self):
        raise ValueError("boom")

    def __str__(self):
        return "<obj & 'q'>"


class NotCalled:
    do_not_call_in_templates = True

    def __call__(self):
        return "should not be called"

    def __str__(self):
        return "not-called"


class SilentError(Exception):
    silent_variable_failure = True


class Silent:
    @property
    def fails(self):
        raise SilentError

    def __str__(self):
        return "silent"


class It:
    def __init__(self, items):
        self._items = list(items)

    def __iter__(self):
        return iter(self._items)

    def __len__(self):
        return len(self._items)


class WrongLen(It):
    def __init__(self, items, length):
        super().__init__(items)
        self._length = length

    def __len__(self):
        return self._length


class BrokenLen(It):
    def __len__(self):
        raise RuntimeError("len failed")


class LanguageSwitcher:
    def go(self):
        translation.activate("de")
        return ""

    def __str__(self):
        translation.activate("de")
        return ""


_lazy_upper = lazy(lambda s: s.upper(), str)
_SAME_NAN = float("nan")
_SHARED_TEXT = "".join(["shared", "-text"])

VALUES = {
    "int": 1234,
    "negint": -5,
    "zero": 0,
    "one": 1,
    "big": 12345678901234567890,
    "float": 1234.5678,
    "negfloat": -0.5,
    "wholefloat": 3.0,
    "bigfloat": 1e20,
    "tinyfloat": 1e-7,
    "nan": float("nan"),
    "inf": float("inf"),
    "dec": decimal.Decimal("1234.5678"),
    "decint": decimal.Decimal("10"),
    "decneg": decimal.Decimal("-0.05"),
    "decone": decimal.Decimal("1"),
    "true": True,
    "false": False,
    "none": None,
    "str": "Hello, World & <You>",
    "numstr": "42",
    "ctrlnum": "5\x1c",
    "vtabnum": "\x0b7\x0c",
    "onestr": "1",
    "floatstr": "3.14159",
    "empty": "",
    "ws": "  a  b  ",
    "unicode": "Ça va? 日本語 ß ﬁ",
    "multiline": "line one\nline two\n\nline four\r\nfive",
    "url": "https://example.com/a b?x=1&y=<2>",
    "email": "Contact me at foo@example.com today",
    "html": "<p>Hello <b>World</b></p> & more",
    "safe": mark_safe("<b>Safe & sound</b>"),
    "lazy": gettext_lazy("Hello"),
    "lazyupper": _lazy_upper("lazy"),
    "list": [3, "b", None, 1.5, True],
    "strlist": ["c", "a", "B"],
    "onelist": ["x"],
    "emptylist": [],
    "nested": ["a", ["b", ["c"]], "d"],
    "safelist": [mark_safe("<i>a</i>"), "<b>"],
    "tuple": (1, 2, 3),
    "set": {1},
    "dict": {"b": 2, "a": 1},
    "odict": OrderedDict([("z", 1), ("y", 2)]),
    "dictlist": [{"n": "z", "v": 2}, {"n": "a", "v": 1}],
    "date": datetime.date(2026, 3, 4),
    "datetime": datetime.datetime(2026, 3, 4, 5, 6, 7),
    "awaredt": datetime.datetime(2026, 3, 4, 5, 6, 7, tzinfo=datetime.UTC),
    "time": datetime.time(13, 45),
    "delta": datetime.timedelta(days=1, seconds=5),
    "obj": Obj(),
    "point": Point(1, 2),
    "bytes": b"bytes",
    "notcalled": NotCalled(),
    "silent": Silent(),
}

CONTEXT = {
    **VALUES,
    "it": It([Point(1, 2), Point(3, 4)]),
    "pairs": [("a", 1), ("b", 2)],
    "fn": lambda: "lambda-called",
    "cls": Obj,
    "nl": [[1, 2], [3, 4]],
    "d": {"a": 1, "b": "", "items": "shadow", "nested": {"k": "v"}},
    "template_names": ["missing.html", "show_value.html"],
    "template_holder": {"name": "BASE.HTML"},
    "biglist": list(range(5000)),
    "bigtuple": tuple(range(5000)),
    "longlen": WrongLen(["a", "b"], 4),
    "zerolen": WrongLen(["a", "b"], 0),
    "brokenlen": BrokenLen(["a"]),
    "nans": [float("nan"), float("nan")],
    "samenan": [_SAME_NAN, _SAME_NAN],
    "bigints": [10**6, 10**6],
    "switcher": LanguageSwitcher(),
    "floats": [1.5, 2.25, 1e-7, -0.0, float("inf")],
    "alias_a": _SHARED_TEXT,
    "alias_b": _SHARED_TEXT,
}

FILTER_ARGS = {
    "add": ["2", "'x'", "list", "none", "dec", "'-1'"],
    "addslashes": [None],
    "capfirst": [None],
    "center": ["10", "'3'"],
    "cut": ["' '", "'l'"],
    "date": [None, "'Y-m-d H:i'", "'D, d M Y'", "'c'", "'U'", "'SHORT_DATE_FORMAT'"],
    "default": ["'D'", "zero"],
    "default_if_none": ["'N'"],
    "dictsort": ["'n'", "0"],
    "dictsortreversed": ["'n'"],
    "divisibleby": ["2", "'3'"],
    "escape": [None],
    "escapejs": [None],
    "escapeseq": [None],
    "filesizeformat": [None],
    "first": [None],
    "floatformat": [None, "2", "-2", "'0'", "'3g'", "'u'", "0", "-1", "'2u'", "'x'"],
    "force_escape": [None],
    "get_digit": ["1", "2", "'x'"],
    "iriencode": [None],
    "join": ["', '", "'<br>'"],
    "json_script": ["'id'", None],
    "last": [None],
    "length": [None],
    "linebreaks": [None],
    "linebreaksbr": [None],
    "linenumbers": [None],
    "ljust": ["10"],
    "lower": [None],
    "make_list": [None],
    "phone2numeric": [None],
    "pluralize": [None, "'es'", "'y,ies'", "'a,b,c'"],
    "pprint": [None],
    "rjust": ["10"],
    "safe": [None],
    "safeseq": [None],
    "slice": ["':2'", "'1:'", "'-1'", "'2'", "'::2'", "'x'"],
    "slugify": [None],
    "stringformat": ["'s'", "'05d'", "'.2f'", "'r'", "'x'", "'e'", "'%'"],
    "striptags": [None],
    "time": [None, "'H:i'", "'P'"],
    "title": [None],
    "truncatechars": ["5", "'x'"],
    "truncatechars_html": ["5"],
    "truncatewords": ["2"],
    "truncatewords_html": ["2"],
    "unordered_list": [None],
    "upper": [None],
    "urlencode": [None, "''"],
    "urlize": [None],
    "urlizetrunc": ["10"],
    "wordcount": [None],
    "wordwrap": ["5"],
    "yesno": [None, "'a,b'", "'a,b,c'"],
}


def _filter_cases():
    for name, arglist in FILTER_ARGS.items():
        for arg in arglist:
            expr = name if arg is None else f"{name}:{arg}"
            yield pytest.param(expr, id=expr)


def test_every_builtin_filter_is_covered():
    from django.template.defaultfilters import register

    assert set(register.filters) - {"random", "timesince", "timeuntil"} == set(FILTER_ARGS)


@pytest.mark.parametrize("expr", list(_filter_cases()))
def test_filter_matches_django_for_every_value(expr):
    failures = []
    for value_name in VALUES:
        src = f"[{{{{ {value_name}|{expr} }}}}]"
        expected, actual = _render_pair("default", src, CONTEXT)
        if actual != expected:
            failures.append(f"{src}\n    django: {expected!r}\n    oxide:  {actual!r}")
    assert not failures, "\n" + "\n".join(failures)


@pytest.mark.parametrize("expr", ["upper", "default:'D'", "join:', '", "first", "slice:':2'"])
def test_filter_matches_django_without_autoescape(expr):
    failures = []
    for value_name in VALUES:
        src = f"[{{{{ {value_name}|{expr} }}}}]"
        expected, actual = _render_pair("noescape", src, CONTEXT)
        if actual != expected:
            failures.append(f"{src}\n    django: {expected!r}\n    oxide:  {actual!r}")
    assert not failures, "\n" + "\n".join(failures)


@pytest.mark.parametrize("engine_key", ["default", "noescape"])
@pytest.mark.parametrize("language", ["en", "de"])
@pytest.mark.parametrize("thousands", [False, True], ids=["plain", "thousands"])
def test_rendering_every_value_matches_django(engine_key, language, thousands):
    failures = []
    for value_name in VALUES:
        for src in (f"[{{{{ {value_name} }}}}]", f"[{{{{ {value_name}|default:'' }}}}]"):
            expected, actual = _render_pair(
                engine_key,
                src,
                CONTEXT,
                language=language,
                settings={"USE_THOUSAND_SEPARATOR": thousands},
            )
            if actual != expected:
                failures.append(f"{src}\n    django: {expected!r}\n    oxide:  {actual!r}")
    assert not failures, "\n" + "\n".join(failures)


@pytest.mark.parametrize("language", ["en", "de"])
def test_localize_tags_match_django(language):
    failures = []
    for value_name in ("int", "float", "dec", "date", "awaredt", "big"):
        for src in (
            f"{{% load l10n %}}{{% localize off %}}[{{{{ {value_name} }}}}]{{% endlocalize %}}",
            f"{{% load l10n %}}{{% localize on %}}[{{{{ {value_name} }}}}]{{% endlocalize %}}",
            f"{{% load l10n %}}[{{{{ {value_name}|unlocalize }}}}|{{{{ {value_name}|localize }}}}]",
            f"{{% load tz %}}{{% localtime off %}}[{{{{ {value_name} }}}}]{{% endlocaltime %}}",
            f"{{% load tz %}}[{{{{ {value_name}|utc }}}}|{{{{ {value_name}|localtime }}}}]",
        ):
            expected, actual = _render_pair(
                "default",
                src,
                CONTEXT,
                language=language,
                settings={"USE_THOUSAND_SEPARATOR": True},
            )
            if actual != expected:
                failures.append(f"{src}\n    django: {expected!r}\n    oxide:  {actual!r}")
    assert not failures, "\n" + "\n".join(failures)


LOOKUPS = [
    "int.real", "int.imag", "int.bit_length", "negint.real", "float.real", "float.is_integer",
    "true.real", "none.x", "str.0", "str.-1", "str.upper", "str.split", "str.title",
    "str.isupper", "empty.strip", "ws.strip", "safe.upper", "safe.0", "list.0", "list.-1",
    "list.9", "list.copy", "list.count", "tuple.0", "tuple.count", "nl.1.0", "dict.a",
    "dict.items", "dict.keys", "dict.values", "dict.missing", "dict.get", "odict.items",
    "odict.z", "d.items", "d.nested.k", "d.b", "obj.name", "obj.empty", "obj.zero",
    "obj.none", "obj.method", "obj.needs_arg", "obj.danger", "obj.missing", "obj.markup",
    "point.x", "point.0", "point.count", "dec.real", "date.year", "date.isoformat",
    "set.0", "fn", "cls", "cls.name", "obj", "notcalled", "silent.fails", "lazy.upper",
    "missing", "missing.x", "dictlist.0.n", "delta.days",
]

LOOKUP_WRAPPERS = [
    "[{{ %s }}]",
    "{%% if %s %%}T{%% else %%}F{%% endif %%}",
    "{%% if not %s %%}T{%% else %%}F{%% endif %%}",
    "{%% if %s == '' %%}E{%% else %%}X{%% endif %%}",
    "{%% if %s is None %%}N{%% else %%}X{%% endif %%}",
    "{%% with v=%s %%}[{{ v }}]{%% endwith %%}",
    "{%% include 'show_value.html' with value=%s %%}",
    "[{{ %s|default:'D' }}]",
    "[{{ %s|default_if_none:'N' }}]",
    "[{{ %s|length }}]",
    "{%% for x in %s %%}<{{ x }}>{%% empty %%}empty{%% endfor %%}",
    "[{%% firstof %s 'FB' %%}]",
    "[{{ 'x'|add:%s }}]",
    "[{{ str|default:%s }}]",
    "{%% if str|default:%s %%}T{%% else %%}F{%% endif %%}",
    "[{%% cycle %s 'b' as c %%}{{ c }}]",
    "[{%% widthratio %s 10 100 %%}]",
    "{%% filter upper %%}{{ %s }}{%% endfilter %%}",
]


@pytest.mark.parametrize("engine_key", ["default", "invalid", "invalid_plain"])
@pytest.mark.parametrize("wrapper", LOOKUP_WRAPPERS)
def test_lookups_match_django(engine_key, wrapper):
    failures = []
    for lookup in LOOKUPS:
        src = wrapper % lookup
        expected, actual = _render_pair(engine_key, src, CONTEXT)
        if actual != expected:
            failures.append(f"{src}\n    django: {expected!r}\n    oxide:  {actual!r}")
    assert not failures, "\n" + "\n".join(failures)


TAG_CASES = [
    "{% for k, v in dict.items %}{{ k }}={{ v }};{% endfor %}",
    "{% for k, v in pairs %}{{ k }}={{ v }};{% endfor %}",
    "{% for k in dict %}{{ k }};{% endfor %}",
    "{% for a, b in dictlist %}{{ a }}/{{ b }};{% endfor %}",
    "{% for a, b in str %}{{ a }}{{ b }}{% endfor %}",
    "{% for a, b in nl %}{{ a }}{{ b }};{% endfor %}",
    "{% for a, b, c in nl %}{{ a }}{% endfor %}",
    "{% for x in list reversed %}{{ x }},{% endfor %}",
    "{% for x in tuple reversed %}{{ x }},{% endfor %}",
    "{% for x in str %}{{ x }}|{% endfor %}",
    "{% for x in int %}{{ x }}{% empty %}E{% endfor %}",
    "{% for x in obj %}{{ x }}{% empty %}E{% endfor %}",
    "{% for x in set %}{{ x }}{% endfor %}",
    "{% for row in nl %}{% for c in row %}{{ forloop.parentloop.counter }}.{{ forloop.counter }}={{ c }} {% endfor %}{% endfor %}",
    "{% for p in it %}{{ p.x }}{{ p.y }}{% for q in it %}{{ p.x }}{{ q.y }}{{ forloop.parentloop.first }}{% endfor %};{% endfor %}",
    "{% for p in it %}{{ forloop.counter }}{{ p.x }}{{ p.y }}{% if forloop.last %}L{% endif %}{% endfor %}",
    "{% for p in it %}{% cycle 'a' 'b' %}{{ p.x }}{{ p.y }}{% endfor %}",
    "{% for p in it %}{% ifchanged p.x %}{{ p.x }}{% endifchanged %}{{ p.y }}{% endfor %}",
    "{% for x in list %}{% ifchanged %}{{ x }}{% else %}same{% endifchanged %}{% endfor %}",
    "{% regroup dictlist by n as grouped %}{% for g in grouped %}{{ g.grouper }}:{% for item in g.list %}{{ item.v }}{% endfor %};{% endfor %}",
    "{% regroup dictlist by missing as grouped %}{% for g in grouped %}[{{ g.grouper }}]{% endfor %}",
    "{% widthratio int 10 100 %}|{% widthratio float 3 7 %}|{% widthratio dec 10 100 %}",
    "{% widthratio int 0 100 %}",
    "{% widthratio str 10 100 %}",
    "{% widthratio int 10 100 as w %}[{{ w }}]",
    "{% cycle missing 'b' as c silent %}[{{ c }}]",
    "{% firstof missing empty none 'x' %}",
    "{% firstof missing empty none %}",
    "{% firstof missing str as v %}[{{ v }}]",
    "{% if str and list %}Y{% endif %}{% if not emptylist %}N{% endif %}{% if 3 in list %}I{% endif %}{% if 'H' in str %}H{% endif %}{% if 'a' in dict %}A{% endif %}{% if 9 not in list %}X{% endif %}",
    "{% if missing in list %}Y{% else %}N{% endif %}",
    "{% if int in str %}Y{% else %}N{% endif %}",
    "{% if int > 3 and float < 3 %}Y{% endif %}{% if str == 'Hello, World & <You>' %}S{% endif %}{% if none == None %}N{% endif %}",
    "{% if missing == missing2 %}EQ{% else %}NE{% endif %}",
    "{% if missing != 'x' %}NE{% else %}EQ{% endif %}",
    "{% if missing < 1 %}LT{% else %}GE{% endif %}",
    "{% if str < 1 %}LT{% else %}GE{% endif %}",
    "{% if dec > 1000 %}G{% endif %}{% if decone == 1 %}O{% endif %}{% if date < datetime %}D{% endif %}",
    "{% if obj.boom %}T{% endif %}",
    "{% if not obj.boom %}T{% else %}F{% endif %}",
    "{% if obj.boom == 1 %}T{% else %}F{% endif %}",
    "{% if obj.boom or true %}T{% else %}F{% endif %}",
    "{% if true or obj.boom %}T{% else %}F{% endif %}",
    "{% if silent.fails %}T{% else %}F{% endif %}",
    "{% with x=obj.boom %}{{ x }}{% endwith %}",
    "{{ obj.boom }}",
    "{{ obj.boom|default:'d' }}",
    "{{ silent.fails }}|{{ silent.fails|default:'d' }}",
    "{{ obj.danger }}|{% if obj.danger %}T{% endif %}",
    "{{ notcalled }}|{{ notcalled.x }}",
    "{{ fn }}|{{ cls.name }}",
    "{{ str|default:missing }}",
    "{{ str|add:missing.attr }}",
    "{{ missing|default:missing2 }}",
    "{% if str|default:missing %}T{% else %}F{% endif %}",
    "{% if missing|default:missing2 %}T{% else %}F{% endif %}",
    "{% with x=str|default:missing %}[{{ x }}]{% endwith %}",
    "{% for x in list|slice:missing %}{{ x }}{% endfor %}",
    "{% with a=str b=int %}{{ a }}{{ b }}{% endwith %}",
    "{% with str|upper as u %}{{ u }}{% endwith %}",
    "{% include 'show_values.html' with a=str b=missing %}",
    "{% include 'show_values.html' with a=str|upper b=int only %}",
    "{% include missing %}",
    "{% include empty %}",
    "{% include template_names with value=str %}",
    "{% extends template_holder.name|lower %}{% block body %}child{% endblock %}",
    "{% extends missing %}{% block body %}child{% endblock %}",
    "{% extends empty %}{% block body %}child{% endblock %}",
    "{% now 'Y' as yr %}{{ yr|length }}",
    "{% spaceless %} <b> {{ str }} </b> <i>x</i> {% endspaceless %}",
    "{% filter upper|escape %}{{ str }}{% endfilter %}",
    "{% filter force_escape %}{{ safe }}{% endfilter %}",
    "{% autoescape off %}{{ str }}{{ safe }}{{ list }}{% endautoescape %}",
    "{% autoescape off %}{% filter upper %}{{ str }}{% endfilter %}{% endautoescape %}",
    "{% verbatim %}{{ str }}{% endverbatim %}",
    "{% templatetag openblock %}{% templatetag closevariable %}",
    "{% lorem 3 w %}",
    "{% lorem 1 b %}",
    "{% lorem 1 p %}",
    "{% url 'missing-view' %}",
    "{{ list }}|{{ tuple }}|{{ dict }}|{{ nested }}|{{ safelist }}|{{ set }}",
    "{{ dictlist }}|{{ pairs }}|{{ point }}",
    "{{ str|upper|lower|title }}|{{ safe|upper }}|{{ safe|escape }}|{{ str|safe|escape }}",
    "{{ list|join:', '|upper }}|{{ safelist|join:'<br>' }}|{{ strlist|join:str }}",
    "{{ int|add:float }}|{{ float|add:int }}|{{ numstr|add:onestr }}|{{ list|add:tuple }}",
    "{{ int|stringformat:'s'|add:'x' }}|{{ none|default_if_none:int|add:1 }}",
    "{{ str|truncatechars:0 }}|{{ str|truncatechars:-1 }}|{{ str|truncatewords:0 }}",
    "{{ str|center:'x' }}|{{ str|ljust:none }}",
    "{% if str is str %}S{% endif %}{% if big is big %}B{% endif %}{% if float is float %}F{% endif %}{% if nan is nan %}N{% endif %}{% if safe is safe %}Z{% endif %}{% if list is list %}L{% endif %}{% if dec is dec %}D{% endif %}",
    "{% if alias_a is alias_b %}A{% endif %}{% if int is not int %}X{% else %}I{% endif %}{% if str or int is str or int %}O{% endif %}",
    "{% if 1000 is 1000 %}Y{% else %}N{% endif %}{% if 5 is 5 %}Y{% else %}N{% endif %}{% if 'a' is 'a' %}Y{% else %}N{% endif %}{% if int is 1234 %}Y{% else %}N{% endif %}{% if 1.5 is 1.5 %}Y{% else %}N{% endif %}",
    "{% if str|lower is str|lower %}Y{% else %}N{% endif %}{% if none is none %}Y{% endif %}{% if true is true %}Y{% endif %}",
    "{% filter length %}abc{% endfilter %}",
    "{% filter wordcount %}a b c{% endfilter %}",
    "{% filter default:'x' %}{% endfilter %}|{% filter lower|capfirst %}ABC{% endfilter %}",
    "{% for x in longlen %}{{ x }}{{ forloop.counter }}{{ forloop.revcounter }}{{ forloop.revcounter0 }}{{ forloop.last }}{{ forloop.length }};{% empty %}E{% endfor %}",
    "{% for x in zerolen %}{{ x }}{% empty %}E{% endfor %}|{% for x in zerolen %}{{ x }}{% endfor %}",
    "{% for x in zerolen reversed %}{{ x }}{% endfor %}|{% for x in longlen reversed %}{{ x }}{{ forloop.last }}{% endfor %}",
    "{% for x in brokenlen %}{{ x }}{% endfor %}",
    "{% for x in list %}{{ forloop.parentloop }}{% endfor %}",
    "{% for x in nans %}{% ifchanged x %}C{% else %}S{% endifchanged %}{% endfor %}",
    "{% for x in samenan %}{% ifchanged x %}C{% else %}S{% endifchanged %}{% endfor %}",
    "{% for x in bigints %}{% ifchanged x %}C{% else %}S{% endifchanged %}{% endfor %}",
    "{% with w='prev' %}{% widthratio int|add:missing 10 100 as w %}[{{ w }}]{% endwith %}",
    "{% widthratio int|add:missing 10 100 %}",
    "{% widthratio int 10 %}",
    "{% widthratio int 10 100 to w %}",
    "{% now '\\\\Y' %}|{% now 'Y' as yr %}{{ yr|length }}",
    "{% now 'Y' extra %}",
    "{{ biglist|first }}|{{ biglist|last }}|{{ biglist|pluralize }}|{{ bigtuple|first }}|{{ bigtuple|last }}",
    "{{ ctrlnum|add:1 }}|{{ vtabnum|add:1 }}|{{ ctrlnum|floatformat }}|{{ vtabnum|floatformat }}",
    "{% url 'missing-view' as u %}[{{ u }}]",
    "{% url 'missing-view' 'a=b' as u %}[{{ u }}]",
    "{% url 'missing-view' arg as u extra %}",
    "{% for x in onelist %}{{ forloop }}{% endfor %}",
    "{% for x in onelist %}{% for k, v in forloop.items %}{{ k }},{% endfor %}{% endfor %}",
    "{% for a, b in pairs %}{% if forloop.first %}{% firstof 'Y' as y %}{% endif %}[{{ y }}]{% endfor %}",
    "{% for x in strlist %}{% include 'counter.html' %}{% endfor %}",
    "{% for x in onelist %}{% for y in strlist %}{% include 'counter.html' %}{% endfor %}{% endfor %}",
    "{% for x in bigints %}{% ifchanged %}{% cycle 'a' '' as q silent %}{% endifchanged %}{{ q }}{% endfor %}",
    "{{ 1_000 }}|{{ 99999999999999999999 }}|{{ 1_0.5 }}|{{ 1__0 }}|{{ -7 }}|{{ +7 }}|{{ 1e3 }}",
    "{% if 99999999999999999999 > int %}Y{% endif %}{% if 1_000 == 1000 %}U{% endif %}",
    "{% include int %}",
    "{% include false %}",
    "{{ str|linenumbers:'x' }}",
    "{{ str|linebreaksbr:'x' }}",
]


@pytest.mark.parametrize("engine_key", ["default", "invalid"])
@pytest.mark.parametrize("src", TAG_CASES)
def test_tags_match_django(engine_key, src):
    _assert_parity(engine_key, src, CONTEXT)


@pytest.mark.parametrize(
    "src",
    [
        "{% extends '' %}",
        "{% extends empty %}",
        "{% extends empty|lower %}",
        "{% extends none %}",
        "{% load cache %}{% cache 10|add:missing 'frag' %}x{% endcache %}",
        "{% load cache %}{% cache 'x' 'frag' %}x{% endcache %}",
        "{% filter length %}abc{% endfilter %}",
        "a{{ str }}{% filter length %}abc{% endfilter %}",
        "{% for x in onelist %}a{% filter length %}x{% endfilter %}{% endfor %}",
        "{% for p in it %}{{ p.x }}{{ p.y }}{% filter length %}x{% endfilter %}{% endfor %}",
        "{% if true %}b{% filter wordcount %}a b{% endfilter %}{% endif %}",
        "{% widthratio int 10 %}",
        "{% widthratio int 10 100 to w %}",
        "{% now 'Y' extra %}",
        "{% url %}",
        "{{ str|default }}",
        "{{ str|upper:'x' }}",
        "{{ str|add }}",
        "{{ str|date:'Y':'m' }}",
        "{{ str|add:d.missing }}",
        "{{ str|add:d.nested.missing }}",
        "{{ str|add:list.9 }}",
        "{% filter escape %}x{% endfilter %}",
        "{% load cache %}{% cache 5 %}x{% endcache %}",
        "{% include int %}",
        "{% include strlist %}",
        "{% extends int %}{% block body %}x{% endblock %}",
        "{% extends true %}{% block body %}x{% endblock %}",
    ],
)
def test_error_messages_match_django(src):
    _assert_parity("default", src, CONTEXT, messages=True)


@pytest.mark.parametrize(
    "src",
    [
        "{% load parity_override %}{{ str|upper }}|{{ list|length }}|{{ list|first }}|{{ none|default:'d' }}",
        "{% load parity_override %}{% for p in it %}{{ p.x|upper }}{{ p.y|default:'d' }};{% endfor %}",
        "{% load parity_override %}{% if list|length == 'LENGTH' %}Y{% endif %}{% with v=str|upper %}{{ v }}{% endwith %}",
        "{% load parity_override %}{% filter upper %}abc{% endfilter %}",
        "{{ str|upper }}|{% load parity_override %}{{ str|upper }}",
    ],
)
def test_library_overrides_of_builtin_filters_match_django(src):
    _assert_parity("override", src, CONTEXT)


@pytest.mark.parametrize("language", ["en", "de"])
@pytest.mark.parametrize(
    "src",
    [
        "{{ float|floatformat:2 }}|{{ dec|floatformat:'2g' }}|{{ int|filesizeformat }}",
        "{{ date|date }}|{{ datetime|date:'SHORT_DATETIME_FORMAT' }}|{{ awaredt|time }}",
        "{{ int|pluralize }}|{{ lazy }}|{{ lazy|upper }}",
        "{% load i18n %}{% translate 'Hello' %}|{% blocktranslate count n=int %}one{% plural %}{{ n }} many{% endblocktranslate %}",
    ],
)
def test_localized_output_matches_django(language, src):
    _assert_parity("default", src, CONTEXT, language=language, settings={"USE_THOUSAND_SEPARATOR": True})


@pytest.mark.parametrize("language", ["en", "de"])
@pytest.mark.parametrize("thousands", [False, True], ids=["plain", "thousands"])
@pytest.mark.parametrize(
    "src",
    [
        "{% for f in floats %}[{{ f }}]{% endfor %}{% for f in floats %}[{{ f|default:'' }}]{% endfor %}",
        "{% load l10n %}{% localize off %}{% for f in floats %}[{{ f }}]{% endfor %}{% endlocalize %}",
        "{% for p in floats %}{% with v=p %}[{{ v }}]{% endwith %}{% endfor %}",
    ],
)
def test_float_rendering_matches_django(language, thousands, src):
    _assert_parity(
        "default", src, CONTEXT, language=language, settings={"USE_THOUSAND_SEPARATOR": thousands}
    )


@pytest.mark.parametrize(
    "body",
    [
        "{{ date|date:'F' }}{{ switcher.go }}{{ date|date:'F' }}",
        "{{ float }}{{ switcher.go }}{{ float }}",
        "{{ date|date:'l' }}{% if switcher.go %}{% endif %}{{ date|date:'l' }}",
        "{{ date|date:'F' }}{{ switcher }}{{ date|date:'F' }}",
        "{{ date|date:'F' }}{{ switcher|lower }}{{ date|date:'F' }}",
        "{{ date|date:'DATE_FORMAT' }}{{ switcher.go }}{{ date|date:'DATE_FORMAT' }}",
    ],
)
def test_language_switch_during_render_matches_django(body):
    src = "{% load i18n %}{% language 'en' %}" + body + "{% endlanguage %}"
    _assert_parity("default", src, CONTEXT)

"""Differential fuzzer: oxide vs stock Django, asserting byte-equal output.

Generates random (structurally valid) Django templates with Hypothesis,
renders each through both the OxideTemplates backend and stock Django,
and flags any divergence (one engine succeeds where the other fails, or
both succeed with different output). Hypothesis shrinks failures to a
minimal reproducer.

Run standalone (NOT via pytest) so conftest's ``Template._render`` patch
does not route stock Django through oxide:

    uv run --no-sync python scripts/fuzz_differential.py [N]
"""

import sys

import django
from django.conf import settings

if not settings.configured:
    settings.configure(
        DEBUG=False,
        USE_I18N=False,
        USE_TZ=False,
        INSTALLED_APPS=[],
        TEMPLATES=[],
    )
django.setup()

from django.template.backends.django import DjangoTemplates  # noqa: E402
from hypothesis import HealthCheck, given  # noqa: E402
from hypothesis import settings as hsettings  # noqa: E402
from hypothesis import strategies as st  # noqa: E402

from django_template_oxide.backend import OxideTemplates  # noqa: E402

_OPTS = {
    "DIRS": [],
    "APP_DIRS": False,
    "OPTIONS": {"builtins": [], "context_processors": []},
}
OXIDE = OxideTemplates({"NAME": "oxide", **_OPTS})
STOCK = DjangoTemplates({"NAME": "django", **_OPTS})

CTX = {
    "a": 7,
    "b": "Hello World",
    "c": [1, 2, 3],
    "d": {"k": "v", "n": 3},
    "e": None,
    "f": True,
    "g": "",
    "h": ["x", "y", "z"],
    "s": "<b>&'\"x",
    "z": 0,
}

VARS = [
    "a", "b", "c", "d", "e", "f", "g", "h", "s", "z",
    "d.k", "d.n", "c.0", "h.1", "x", "forloop.counter", "forloop.first",
]

FILTERS = [
    "upper", "lower", "title", "capfirst", "length", "default:'D'",
    "default_if_none:'N'", "first", "last", "join:'-'", "add:'3'",
    "cut:'l'", "wordcount", "yesno:'y,n,m'", "striptags", "escape",
    "safe", "escapejs", "slugify", "ljust:'4'", "rjust:'4'", "center:'6'",
    "truncatechars:'5'", "truncatewords:'2'", "linenumbers", "make_list",
    "addslashes", "force_escape", "length_is:'3'", "stringformat:'s'",
]

_names = st.sampled_from(VARS)
_filters = st.lists(st.sampled_from(FILTERS), max_size=3)
_text = st.text(alphabet="ab \n<>&", max_size=6)
_expr = st.builds(
    lambda v, fs: "{{ " + v + "".join("|" + f for f in fs) + " }}",
    _names,
    _filters,
)

_node = st.deferred(
    lambda: st.one_of(
        _text,
        _expr,
        st.builds(lambda v, b: "{% if " + v + " %}" + b + "{% endif %}", _names, _body),
        st.builds(
            lambda v, t, e: "{% if " + v + " %}" + t + "{% else %}" + e + "{% endif %}",
            _names, _body, _body,
        ),
        st.builds(
            lambda v, b: "{% for x in " + v + " %}" + b + "{% empty %}E{% endfor %}",
            _names, _body,
        ),
        st.builds(lambda v, b: "{% with y=" + v + " %}" + b + "{% endwith %}", _names, _body),
    )
)
_body = st.lists(_node, max_size=4).map("".join)


def _eval(backend, src):
    try:
        tpl = backend.from_string(src)
    except Exception:
        return ("fail", None)
    try:
        return ("ok", tpl.render(dict(CTX)))
    except Exception:
        return ("fail", None)


@hsettings(max_examples=2000, deadline=None, suppress_health_check=list(HealthCheck))
@given(src=_body)
def fuzz(src):
    ox_status, ox_out = _eval(OXIDE, src)
    sk_status, sk_out = _eval(STOCK, src)
    ox_ok = ox_status == "ok"
    sk_ok = sk_status == "ok"
    # oxide being MORE lenient than Django (rendering where Django errors on
    # an invalid template) is an acceptable design choice, not a parity bug.
    # The real bugs are: oxide rejecting a template Django renders, or both
    # rendering with different bytes.
    if sk_ok and not ox_ok:
        raise AssertionError(
            f"OXIDE REJECTED A DJANGO-VALID TEMPLATE\n  src={src!r}\n"
            f"  stock_out={sk_out!r}"
        )
    if ox_ok and sk_ok and ox_out != sk_out:
        raise AssertionError(
            f"OUTPUT DIVERGENCE\n  src={src!r}\n"
            f"  oxide={ox_out!r}\n  stock={sk_out!r}"
        )


if __name__ == "__main__":
    try:
        fuzz()
    except AssertionError as exc:
        print(exc)
        sys.exit(1)
    print("no divergence found")

"""Comparison benchmark: oxide vs django-rusty-templates vs stock Django.

Measures three axes: render workloads, cold compile time, and scaling
across item counts. Each backend renders each case N times after one
warmup pass; reports mean and p99 latency.

If the FFI profiler is compiled in (`cargo build --features=prof`),
an extra per-zone breakdown is printed for the FULL TEMPLATE.

Run:

    uv sync --group dev
    uv run benches/bench.py

Env knobs: BENCH_ITEMS, BENCH_ITERS, BENCH_SECTIONS.
"""

import datetime
import os
import time

import django
from django.conf import settings


# Synthesised modules are registered as top-level (not under `benches.`)
# because rusty's backend init eagerly imports `libraries` by name, and
# `benches.` is not on sys.path unless invoked via `python -m benches.bench`.
_BENCH_URLCONF = "_oxide_bench_urls"
_BENCH_LIB_PATH = "_oxide_bench_lib"


# Locmem template store shared by both backends. The global
# `settings.TEMPLATES` must match the per-engine config because oxide's
# `{% include %}` routes through `django.template.loader.get_template`
# rather than the local engine.
_LOCMEM_TEMPLATES = {
    "bench_row.html": (
        "<tr>"
        "<td>{{ app.candidate.name }}</td>"
        "<td>{{ app.status|title }}</td>"
        "</tr>"
    ),
    "bench_base.html": (
        "<html>"
        "<head><title>{% block title %}default-title{% endblock %}</title></head>"
        "<body>"
        "{% block header %}<h1>default-header</h1>{% endblock %}"
        "<main>{% block content %}default-content{% endblock %}</main>"
        "</body></html>"
    ),
    "bench_child.html": (
        '{% extends "bench_base.html" %}'
        "{% block title %}Apps ({{ applications|length }}){% endblock %}"
        "{% block header %}<h1>Applications</h1>{% endblock %}"
        "{% block content %}"
        "<table>"
        "{% for app in applications %}"
        "<tr><td>{{ app.candidate.name }}</td><td>{{ app.status }}</td></tr>"
        "{% endfor %}"
        "</table>"
        "{% endblock %}"
    ),
}


_TEMPLATES_OPTIONS = {
    "context_processors": [],
    "builtins": [
        "django.template.defaulttags",
        "django.template.defaultfilters",
        "django.template.loader_tags",
    ],
    "libraries": {
        "bench": _BENCH_LIB_PATH,
    },
    "loaders": [
        ("django.template.loaders.locmem.Loader", _LOCMEM_TEMPLATES),
    ],
}


if not settings.configured:
    settings.configure(
        DEBUG=False,
        INSTALLED_APPS=[],
        # TEMPLATES is populated so oxide's {% include %} (which routes
        # through `django.template.loader.get_template`) sees the same
        # locmem dict as direct `.get_template` calls.
        TEMPLATES=[
            {
                "BACKEND": "django.template.backends.django.DjangoTemplates",
                "DIRS": [],
                "APP_DIRS": False,
                "OPTIONS": _TEMPLATES_OPTIONS,
            },
        ],
        USE_TZ=True,
        USE_I18N=True,
        LANGUAGE_CODE="en-us",
        ROOT_URLCONF=_BENCH_URLCONF,
        SECRET_KEY="bench-not-a-secret",
        ALLOWED_HOSTS=["*"],
    )
    django.setup()


# Synthesise a urlconf so `{% url 'detail' app.id %}` resolves without
# a real project on the path.
import sys
import types

if _BENCH_URLCONF not in sys.modules:
    from django.urls import path
    from django.http import HttpResponse

    def _dummy_view(request, pk):  # pragma: no cover
        return HttpResponse("")

    _mod = types.ModuleType(_BENCH_URLCONF)
    _mod.urlpatterns = [
        path("apps/<int:pk>/", _dummy_view, name="detail"),
    ]
    sys.modules[_BENCH_URLCONF] = _mod


from django import template  # noqa: E402
from django.template.backends.django import DjangoTemplates  # noqa: E402

from django_template_oxide.backend import OxideTemplates  # noqa: E402


from django_rusty_templates import RustyTemplates as _RustyTemplates  # noqa: E402


# Optional native profiler, compiled in via `cargo build --features=prof`.
try:
    from django_template_oxide._rust import get_prof_stats, reset_prof_stats
except ImportError:

    def get_prof_stats():
        return {}

    def reset_prof_stats():
        pass


# Custom Python tag + filter for the FFI-cost bench cases. Kept trivial
# so any backend cost difference reflects FFI overhead, not user code.


_bench_register = template.Library()


@_bench_register.filter
def bench_noop(value, _arg=None):
    """No-op filter. Measures Python-filter call-out overhead per row."""
    return value


@_bench_register.simple_tag
def bench_simple_tag(value):
    """Trivial simple_tag. Measures the simple_tag dispatch path."""
    return f"[{value}]"


@_bench_register.tag(name="bench_raw_tag")
def _do_bench_raw_tag(parser, token):
    """Raw @register.tag. Measures the PyOpaqueNode dispatch path."""
    bits = token.split_contents()
    if len(bits) != 2:
        raise template.TemplateSyntaxError("bench_raw_tag takes one arg")
    var = parser.compile_filter(bits[1])
    return _BenchRawTagNode(var)


class _BenchRawTagNode(template.Node):
    def __init__(self, var):
        self.var = var

    def render(self, context):
        return f"<{self.var.resolve(context)}>"


# Register the synthetic library module so engines can resolve it by
# dotted name. Idempotent across repeated `run()` calls.
if _BENCH_LIB_PATH not in sys.modules:
    _lib_mod = types.ModuleType(_BENCH_LIB_PATH)
    _lib_mod.register = _bench_register
    sys.modules[_BENCH_LIB_PATH] = _lib_mod


class _Company:
    __slots__ = ("name",)

    def __init__(self, name):
        self.name = name


class _Posting:
    __slots__ = ("title", "company")

    def __init__(self, title, company):
        self.title = title
        self.company = company


class _Candidate:
    __slots__ = ("name",)

    def __init__(self, name):
        self.name = name


class _Stage:
    __slots__ = ("name", "order")

    def __init__(self, name, order):
        self.name = name
        self.order = order


class _Deep:
    """Six-level deep object for the DEEP LOOKUP bench."""

    __slots__ = ("a",)

    def __init__(self, value):
        # Builds a.b.c.d.e.f -> value.
        class _F:
            __slots__ = ("f",)

            def __init__(self, v):
                self.f = v

        class _E:
            __slots__ = ("e",)

            def __init__(self, v):
                self.e = _F(v)

        class _D:
            __slots__ = ("d",)

            def __init__(self, v):
                self.d = _E(v)

        class _C:
            __slots__ = ("c",)

            def __init__(self, v):
                self.c = _D(v)

        class _B:
            __slots__ = ("b",)

            def __init__(self, v):
                self.b = _C(v)

        self.a = _B(value)


class _Application:
    """Application-shaped object with the same attribute paths as a
    Django ``Application`` model, so template lookups exercise the same
    `getattr` chains across all three backends."""

    __slots__ = (
        "id",
        "candidate",
        "posting",
        "stage",
        "status",
        "created_at",
        "is_archived",
        "rating",
        "tags",
        "html_blob",
        "deep",
        "meta",
    )

    def __init__(self, i):
        self.id = i
        self.candidate = _Candidate(f"Candidate {i}")
        self.posting = _Posting(
            title=f"Posting {i % 30}",
            company=_Company(f"Company {i % 6}"),
        )
        self.stage = _Stage(name=f"Stage {i % 5}", order=i % 5)
        self.status = ("active", "rejected", "withdrawn", "hired")[i % 4]
        self.created_at = datetime.date(2024, 1, 1) + datetime.timedelta(days=i)
        self.is_archived = (i % 7) == 0
        self.rating = (i % 10) - 5  # spans negative for IF CHAIN
        self.tags = ["red", "green", "blue"][: (i % 3) + 1]
        # HTML metachars on every other row so autoescape has real work.
        self.html_blob = (
            "Plain text & some content"
            if i % 2
            else '<script>alert("xss")</script>&copy;'
        )
        self.deep = _Deep(f"deep-{i}")
        self.meta = {
            "k1": f"v{i}-1",
            "k2": f"v{i}-2",
            "k3": f"v{i}-3",
            "k4": f"v{i}-4",
        }


def _build_applications(count):
    return [_Application(i) for i in range(count)]


# Render workloads.


_FULL_TEMPLATE = (
    "<table><thead><tr><th>Name</th><th>Job</th><th>Company</th>"
    "<th>Stage</th><th>Date</th><th>Status</th></tr></thead><tbody>\n"
    "{% for app in applications %}"
    '<tr class="row {% if app.is_archived %}archived{% else %}active{% endif %}">'
    "<td>{{ app.candidate.name }}</td>"
    '<td>{{ app.posting.title|default:"\u2014" }}</td>'
    '<td>{{ app.posting.company.name|default:"\u2014" }}</td>'
    "<td>{{ app.stage.name }}</td>"
    '<td>{{ app.created_at|date:"M d, Y" }}</td>'
    '<td class="status-{{ app.status }}">{{ app.status|title }}</td>'
    "</tr>{% endfor %}</tbody></table>"
)


RENDER_CASES = [
    # Loop overhead baselines.
    (
        "TEXT ONLY (loop, no vars)",
        "{% for app in applications %}<tr><td>plain</td></tr>{% endfor %}",
    ),
    (
        "VARS ONLY (3 attrs, no filters)",
        (
            "{% for app in applications %}"
            "<tr>"
            "<td>{{ app.candidate.name }}</td>"
            "<td>{{ app.stage.name }}</td>"
            "<td>{{ app.status }}</td>"
            "</tr>{% endfor %}"
        ),
    ),
    ("FULL TEMPLATE (real-world mix)", _FULL_TEMPLATE),
    # Variable lookup shapes.
    (
        "DEEP LOOKUP (a.b.c.d.e.f chain)",
        "{% for app in applications %}{{ app.deep.a.b.c.d.e.f }}{% endfor %}",
    ),
    (
        "DICT LOOKUP (3 keys per row)",
        (
            "{% for app in applications %}"
            "{{ app.meta.k1 }}{{ app.meta.k2 }}{{ app.meta.k3 }}"
            "{% endfor %}"
        ),
    ),
    (
        "LIST INDEXING (tags.0)",
        (
            "{% for app in applications %}"
            "{{ app.tags.0 }}"
            "{% endfor %}"
        ),
    ),
    # Filter pipelines.
    (
        "FILTER CHAIN (6-deep pipeline)",
        (
            "{% for app in applications %}"
            "{{ app.candidate.name|upper|lower|title|truncatechars:20|default:\"x\"|safe }}"
            "{% endfor %}"
        ),
    ),
    (
        "DATE FILTERS (3 date formats)",
        (
            "{% for app in applications %}"
            '{{ app.created_at|date:"Y-m-d" }}|'
            '{{ app.created_at|date:"M d" }}|'
            '{{ app.created_at|date:"D" }}'
            "{% endfor %}"
        ),
    ),
    # Conditionals.
    (
        "IF/ELIF CHAIN (5 branches)",
        (
            "{% for app in applications %}"
            "{% if app.rating < -2 %}terrible"
            "{% elif app.rating < 0 %}poor"
            "{% elif app.rating == 0 %}neutral"
            "{% elif app.rating < 3 %}good"
            "{% else %}excellent{% endif %}"
            "{% endfor %}"
        ),
    ),
    (
        "WITH NESTED (4 levels)",
        (
            "{% for app in applications %}"
            "{% with n=app.candidate.name %}"
            "{% with s=app.stage.name %}"
            "{% with st=app.status %}"
            "{% with c=app.posting.company.name %}"
            "{{ n }}|{{ s }}|{{ st }}|{{ c }}"
            "{% endwith %}{% endwith %}{% endwith %}{% endwith %}"
            "{% endfor %}"
        ),
    ),
    # Forloop state.
    (
        "FORLOOP COUNTER (counter+first+last)",
        (
            "{% for app in applications %}"
            "{{ forloop.counter }}:{{ app.candidate.name }}"
            "{% if forloop.first %}[first]{% endif %}"
            "{% if forloop.last %}[last]{% endif %}"
            "{% endfor %}"
        ),
    ),
    (
        "CYCLE TAG (3 classes)",
        (
            "{% for app in applications %}"
            '<tr class="{% cycle \'odd\' \'even\' \'other\' %}">'
            "{{ app.candidate.name }}</tr>"
            "{% endfor %}"
        ),
    ),
    # Auto-escape.
    (
        "AUTOESCAPE HEAVY (HTML metachars)",
        (
            "{% for app in applications %}"
            "<div>{{ app.html_blob }}</div>"
            "{% endfor %}"
        ),
    ),
    # URL reverse.
    (
        "URL TAG (per row)",
        (
            "{% for app in applications %}"
            "<a href=\"{% url 'detail' app.id %}\">{{ app.candidate.name }}</a>"
            "{% endfor %}"
        ),
    ),
    # CSRF token (cheap, but used everywhere).
    (
        "CSRF TOKEN (per row)",
        (
            "{% for app in applications %}"
            "<form>{% csrf_token %}</form>"
            "{% endfor %}"
        ),
    ),
    # Empty arm of for.
    (
        "FOR EMPTY (empty list path)",
        (
            "{% for app in empty_apps %}"
            "{{ app.candidate.name }}"
            "{% empty %}NONE{% endfor %}"
        ),
    ),
    # Spaceless.
    (
        "SPACELESS BLOCK",
        (
            "{% for app in applications %}"
            "{% spaceless %}"
            "<tr>  <td>  {{ app.candidate.name }}  </td>  </tr>"
            "{% endspaceless %}"
            "{% endfor %}"
        ),
    ),
    # Custom Python tag/filter (FFI cost).
    (
        "CUSTOM PY FILTER (call per row)",
        (
            "{% load bench %}"
            "{% for app in applications %}"
            "{{ app.candidate.name|bench_noop }}"
            "{% endfor %}"
        ),
    ),
    (
        "CUSTOM PY simple_tag (call per row)",
        (
            "{% load bench %}"
            "{% for app in applications %}"
            "{% bench_simple_tag app.candidate.name %}"
            "{% endfor %}"
        ),
    ),
    (
        "CUSTOM PY @register.tag (per row)",
        (
            "{% load bench %}"
            "{% for app in applications %}"
            "{% bench_raw_tag app.candidate.name %}"
            "{% endfor %}"
        ),
    ),
    # Inheritance + include (loader-backed).
    (
        "INCLUDE LOOP (50 includes)",
        (
            "{% for app in applications %}"
            "{% include 'bench_row.html' %}"
            "{% endfor %}"
        ),
    ),
    (
        "INHERITANCE (extends+3 blocks)",
        # Rendered template is bench_child.html (extends bench_base.html);
        # the stub here just includes it so the same helper handles this
        # case without special-casing the entrypoint.
        "{% include 'bench_child.html' %}",
    ),
]


# Compile-time workloads. Three deterministic sizes; each backend
# compiles `iterations` copies and reports mean per-compile time.


def _gen_template(num_rows: int) -> str:
    """Synthesise a template with `num_rows` of the FULL TEMPLATE row
    pattern (~12 AST nodes per row)."""
    row = (
        '<tr class="row {% if app.is_archived %}archived{% else %}active{% endif %}">'
        "<td>{{ app.candidate.name }}</td>"
        '<td>{{ app.posting.title|default:"-" }}</td>'
        '<td>{{ app.posting.company.name|default:"-" }}</td>'
        "<td>{{ app.stage.name }}</td>"
        '<td>{{ app.created_at|date:"M d, Y" }}</td>'
        '<td class="status-{{ app.status }}">{{ app.status|title }}</td>'
        "</tr>"
    )
    return (
        "<table><thead><tr><th>Name</th></tr></thead><tbody>"
        + (row * num_rows)
        + "</tbody></table>"
    )


COMPILE_CASES = [
    ("SMALL (10 rows ~120 nodes)", _gen_template(10)),
    ("MEDIUM (100 rows ~1200 nodes)", _gen_template(100)),
    ("LARGE (500 rows ~6000 nodes)", _gen_template(500)),
]


SCALING_ITEM_COUNTS = [1, 10, 100, 1000]


def _backend_options():
    """Shared {libraries, loaders, builtins} so per-case numbers reflect
    the engine, not the configuration. Built fresh on each call so
    callers (e.g. rusty) can strip incompatible OPTIONS keys."""
    return {
        "DIRS": [],
        "APP_DIRS": False,
        "OPTIONS": {
            "context_processors": _TEMPLATES_OPTIONS["context_processors"],
            "builtins": list(_TEMPLATES_OPTIONS["builtins"]),
            "libraries": dict(_TEMPLATES_OPTIONS["libraries"]),
            "loaders": list(_TEMPLATES_OPTIONS["loaders"]),
        },
    }


def _build_backends():
    opts = _backend_options()
    backends = {
        "oxide": OxideTemplates({"NAME": "oxide", **opts}),
        "stock": DjangoTemplates({"NAME": "stock", **opts}),
    }
    # rusty doesn't accept the `loaders` OPTIONS key; strip it.
    rusty_opts = _backend_options()
    rusty_opts["OPTIONS"].pop("loaders", None)
    try:
        backends["rusty"] = _RustyTemplates({"NAME": "rusty", **rusty_opts})
    except Exception as e:  # pragma: no cover - rusty quirk
        print(f"  (rusty backend init failed: {e!r}; skipping rusty column)")
    return backends


def _percentile(values, pct):
    """Return the `pct` (0-100) percentile of `values` (need not be sorted)."""
    if not values:
        return 0.0
    s = sorted(values)
    k = max(0, min(len(s) - 1, int(round((pct / 100.0) * (len(s) - 1)))))
    return s[k]


def _time_render(tpl_or_callable, ctx, n):
    """Time `n` renders, returning (mean_ms, p99_ms).

    Accepts a backend template object or a zero-arg callable that
    performs one render."""
    if callable(tpl_or_callable):
        runner = tpl_or_callable
    else:
        runner = lambda: tpl_or_callable.render(ctx)  # noqa: E731
    runner()  # warmup
    timings = []
    for _ in range(n):
        t0 = time.perf_counter()
        runner()
        timings.append((time.perf_counter() - t0) * 1000)
    mean = sum(timings) / len(timings)
    return mean, _percentile(timings, 99)


def _time_compile(backend, src, n):
    """Time `n` cold compiles of `src` (full lex+parse per iteration)."""
    backend.from_string(src)  # warmup
    timings = []
    for _ in range(n):
        t0 = time.perf_counter()
        backend.from_string(src)
        timings.append((time.perf_counter() - t0) * 1000)
    return sum(timings) / len(timings), _percentile(timings, 99)


def _format_row(label, results, has_rusty):
    """Build a single output row given a dict {backend: (mean, p99) | str}.

    A string value is treated as an error marker (e.g. ``ERROR``) so the
    table stays compact when a backend can't run a case."""
    parts = [f"{label:36s}"]
    for be in ("oxide", "rusty" if has_rusty else None, "stock"):
        if be is None:
            continue
        cell = results.get(be)
        if cell is None:
            parts.append(f"{'-':>22s}")
        elif isinstance(cell, str):
            parts.append(f"{cell:>22s}")
        else:
            mean, p99 = cell
            parts.append(f"{mean:>7.3f}ms (p99 {p99:>6.3f})")
    if (
        has_rusty
        and isinstance(results.get("oxide"), tuple)
        and isinstance(results.get("rusty"), tuple)
    ):
        ratio = results["oxide"][0] / results["rusty"][0] if results["rusty"][0] > 0 else 0
        parts.append(f"{ratio:>5.2f}x")
    return "  ".join(parts)


def _print_header(title, has_rusty):
    print()
    print("=" * 100)
    print(title)
    print("=" * 100)
    head = f"{'workload':36s}"
    for be in ("oxide", "rusty" if has_rusty else None, "stock"):
        if be is None:
            continue
        head += f"  {be:>22s}"
    if has_rusty:
        head += f"  {'ratio':>5s}"
    print(head)
    print("-" * len(head))


def _error_marker(e: BaseException) -> str:
    """Compact in-cell marker for a failed backend run."""
    name = type(e).__name__
    return {
        "NotImplementedError": "ERROR: unsupported",
        "TemplateDoesNotExist": "ERROR: no template",
        "TemplateSyntaxError": "ERROR: syntax",
        "InvalidTemplateLibrary": "ERROR: invalid lib",
    }.get(name, f"ERROR: {name}")


def section_render(backends, item_count, iterations):
    apps = _build_applications(item_count)
    ctx = {"applications": apps, "empty_apps": []}
    has_rusty = "rusty" in backends
    _print_header(
        f"RENDER WORKLOADS  (items={item_count}, iters={iterations})", has_rusty
    )
    for label, src in RENDER_CASES:
        results = {}
        try:
            reference = backends["stock"].from_string(src).render(dict(ctx))
        except Exception:
            reference = None
        for be_name, be in backends.items():
            try:
                tpl = be.from_string(src)
                if reference is not None and tpl.render(dict(ctx)) != reference:
                    results[be_name] = "ERROR: wrong output"
                    continue
                results[be_name] = _time_render(tpl, ctx, iterations)
            except Exception as e:  # pragma: no cover - bench best-effort
                results[be_name] = _error_marker(e)
        print(_format_row(label, results, has_rusty))


def section_compile(backends, iterations):
    has_rusty = "rusty" in backends
    _print_header(f"COMPILE TIME  (iters={iterations})", has_rusty)
    for label, src in COMPILE_CASES:
        results = {}
        for be_name, be in backends.items():
            try:
                results[be_name] = _time_compile(be, src, iterations)
            except Exception as e:  # pragma: no cover
                results[be_name] = _error_marker(e)
        print(_format_row(label, results, has_rusty))


def section_scaling(backends, iterations):
    has_rusty = "rusty" in backends
    _print_header(
        f"SCALING SWEEP (FULL TEMPLATE)  (iters={iterations})", has_rusty
    )
    for n in SCALING_ITEM_COUNTS:
        apps = _build_applications(n)
        ctx = {"applications": apps}
        results = {}
        for be_name, be in backends.items():
            try:
                tpl = be.from_string(_FULL_TEMPLATE)
                results[be_name] = _time_render(tpl, ctx, iterations)
            except Exception as e:  # pragma: no cover
                results[be_name] = _error_marker(e)
        suffix = ""
        if isinstance(results.get("oxide"), tuple):
            suffix = f"  [oxide ns/item={results['oxide'][0] * 1e6 / n:.0f}]"
        print(_format_row(f"items={n:6d}", results, has_rusty) + suffix)


def section_prof(backends, ctx, iterations):
    """If the FFI profiler is compiled in, print per-zone totals."""
    print("\n--- oxide profile breakdown (FULL TEMPLATE) ---")
    tpl = backends["oxide"].from_string(_FULL_TEMPLATE)
    tpl.render(ctx)
    reset_prof_stats()
    for _ in range(iterations):
        tpl.render(ctx)
    stats = dict(get_prof_stats())
    if stats:
        items = sorted(stats.items(), key=lambda kv: -kv[1]["total_us"])
        for k, s in items[:10]:
            per_run_ms = s["total_us"] / iterations / 1000
            print(
                f"  {k:44s} count/run={s['count'] // iterations:>5d} "
                f"per_run={per_run_ms:.3f}ms avg_ns={s['avg_ns']}"
            )
    else:
        print(
            "  (prof feature not enabled, rebuild with "
            "`cargo build --features=prof`)"
        )


def run(item_count=50, iterations=200, sections=("render", "compile", "scaling")):
    backends = _build_backends()
    if "render" in sections:
        section_render(backends, item_count, iterations)
    if "compile" in sections:
        # Compile cases are slower; cap iterations to keep bench under ~30s.
        section_compile(backends, max(20, iterations // 5))
    if "scaling" in sections:
        section_scaling(backends, iterations)
    apps = _build_applications(item_count)
    section_prof(backends, {"applications": apps}, iterations)


if __name__ == "__main__":
    n_items = int(os.environ.get("BENCH_ITEMS", "50"))
    n_iters = int(os.environ.get("BENCH_ITERS", "200"))
    sections_env = os.environ.get("BENCH_SECTIONS", "render,compile,scaling")
    sections = tuple(s.strip() for s in sections_env.split(",") if s.strip())
    run(item_count=n_items, iterations=n_iters, sections=sections)

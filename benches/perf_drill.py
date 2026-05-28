"""Focused micro-profiler for cases where rusty beats oxide.

Drills into FOR EMPTY, FORLOOP COUNTER, and COMPILE SMALL. For each,
runs N iterations, captures the prof-zone breakdown (requires
`cargo build --features=prof`), and prints a side-by-side timing.
"""

import os
import sys
import time

# Reuse bench infrastructure (synthesised URLconf, library, settings).
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bench as _bench  # noqa: E402

from django.template.backends.django import DjangoTemplates  # noqa: E402
from django_template_oxide.backend import OxideTemplates  # noqa: E402

try:
    from django_rusty_templates import RustyTemplates
except ImportError:
    RustyTemplates = None

from django_template_oxide._rust import get_prof_stats, reset_prof_stats  # noqa: E402


CASES = {
    "FOR EMPTY": (
        "{% for app in empty_apps %}"
        "{{ app.candidate.name }}"
        "{% empty %}NONE{% endfor %}"
    ),
    "FORLOOP COUNTER": (
        "{% for app in applications %}"
        "{{ forloop.counter }}:{{ app.candidate.name }}"
        "{% if forloop.first %}[first]{% endif %}"
        "{% if forloop.last %}[last]{% endif %}"
        "{% endfor %}"
    ),
}


def _bench_render(backend, src, ctx, n):
    tpl = backend.from_string(src)
    tpl.render(ctx)
    t0 = time.perf_counter()
    for _ in range(n):
        tpl.render(ctx)
    return (time.perf_counter() - t0) * 1000 / n  # ms/render


def _bench_compile(backend, src, n):
    backend.from_string(src)
    t0 = time.perf_counter()
    for _ in range(n):
        backend.from_string(src)
    return (time.perf_counter() - t0) * 1000 / n  # ms/compile


def _print_prof(label, iterations):
    print(f"\n  prof zones for {label} (per render, iters={iterations}):")
    stats = dict(get_prof_stats())
    if not stats:
        print("    (prof feature not enabled)")
        return
    items = sorted(stats.items(), key=lambda kv: -kv[1]["total_us"])
    print(f"    {'zone':50s} {'count/run':>10s} {'per_run_us':>12s} {'avg_ns':>10s}")
    for k, s in items[:15]:
        per_run_us = s["total_us"] / iterations
        count_per_run = s["count"] // iterations
        print(f"    {k:50s} {count_per_run:>10d} {per_run_us:>10.3f}us {s['avg_ns']:>10d}")


def main():
    apps = _bench._build_applications(50)
    ctx = {"applications": apps, "empty_apps": []}

    ox = OxideTemplates({"NAME": "ox", **_bench._backend_options()})
    stk = DjangoTemplates({"NAME": "stk", **_bench._backend_options()})
    rust = None
    if RustyTemplates is not None:
        rust_opts = _bench._backend_options()
        rust_opts["OPTIONS"].pop("loaders", None)
        rust = RustyTemplates({"NAME": "rust", **rust_opts})

    iters = 5000  # high iteration count for stable microbench

    for label, src in CASES.items():
        print(f"\n=== {label} ===")
        ox_ms = _bench_render(ox, src, ctx, iters)
        stk_ms = _bench_render(stk, src, ctx, iters)
        line = f"  oxide={ox_ms*1000:.2f}us  stock={stk_ms*1000:.2f}us"
        if rust is not None:
            rust_ms = _bench_render(rust, src, ctx, iters)
            line += f"  rusty={rust_ms*1000:.2f}us  (oxide/rusty={ox_ms/rust_ms:.2f}x)"
        print(line)

        # Re-run with prof enabled, only for oxide.
        reset_prof_stats()
        tpl = ox.from_string(src)
        tpl.render(ctx)  # warmup
        reset_prof_stats()
        for _ in range(iters):
            tpl.render(ctx)
        _print_prof(label, iters)

    src_small = _bench._gen_template(10)
    n_compile = 500
    print("\n=== COMPILE SMALL ===")
    ox_ms = _bench_compile(ox, src_small, n_compile)
    stk_ms = _bench_compile(stk, src_small, n_compile)
    line = f"  oxide={ox_ms*1000:.1f}us  stock={stk_ms*1000:.1f}us"
    if rust is not None:
        rust_ms = _bench_compile(rust, src_small, n_compile)
        line += f"  rusty={rust_ms*1000:.1f}us  (oxide/rusty={ox_ms/rust_ms:.2f}x)"
    print(line)

    reset_prof_stats()
    for _ in range(n_compile):
        ox.from_string(src_small)
    _print_prof("COMPILE SMALL", n_compile)


if __name__ == "__main__":
    main()

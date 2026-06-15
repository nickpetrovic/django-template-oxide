"""Per-zone micro-profiler driven off the bench workload list.

Two modes:

  * No args  -> a compact summary table of every bench render + compile
    workload (oxide vs rusty, with weak spots flagged). Readable overview.

  * With args -> case-name substring filters. Prints the summary table for
    the matching cases AND the oxide prof-zone breakdown for each (the
    "drill in" view). The zone breakdown needs a prof build.

Build prof, then run:

    VIRTUAL_ENV=.venv uvx --from 'maturin>=1,<2' maturin develop --release --features prof
    uv run --no-sync python benches/perf_drill.py                 # overview
    uv run --no-sync python benches/perf_drill.py NESTED "IF "    # drill in

Env knobs: PERF_ITERS (render iters, default 2000).
"""

import os
import sys
import time

# Reuse bench infrastructure (synthesised URLconf, library, settings,
# template store, workload + object fixtures).
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bench as _bench  # noqa: E402

from django_template_oxide.backend import OxideTemplates  # noqa: E402

try:
    from django_rusty_templates import RustyTemplates
except ImportError:
    RustyTemplates = None

from django_template_oxide._rust import get_prof_stats, reset_prof_stats  # noqa: E402

LABEL_W = 42


def _render_iters():
    return int(os.environ.get("PERF_ITERS", "2000"))


def _time(fn, n):
    fn()  # warmup
    t0 = time.perf_counter()
    for _ in range(n):
        fn()
    return (time.perf_counter() - t0) * 1000 / n  # ms/op


def _matches(label, filters):
    return not filters or any(f.lower() in label.lower() for f in filters)


def _measure(make_runner, iters):
    """Return ms/op, or an 'ERR(...)' / None marker on failure.

    `make_runner` compiles the template (may raise) and returns a zero-arg
    render/compile closure to time."""
    try:
        runner = make_runner()
    except Exception as e:
        return f"ERR({type(e).__name__})"
    try:
        return _time(runner, iters)
    except Exception as e:
        return f"ERR({type(e).__name__})"


def _render_runner(backend, src, ctx):
    def make():
        tpl = backend.from_string(src)
        return lambda: tpl.render(ctx)

    return make


def _compile_runner(backend, src):
    def make():
        backend.from_string(src)
        return lambda: backend.from_string(src)

    return make


def _fmt_us(cell):
    if isinstance(cell, str):
        return cell
    if cell is None:
        return "-"
    return f"{cell * 1000:.2f}us"


def _ratio_and_flag(ox, rusty):
    if not (isinstance(ox, float) and isinstance(rusty, float) and rusty > 0):
        return "-", ""
    r = ox / rusty
    flag = ""
    if r > 1.0:
        flag = "<< SLOWER than rusty"
    elif r >= 0.60:
        flag = "< weak (small lead)"
    return f"{r:.2f}x", flag


def _print_table(title, rows):
    if not rows:
        return
    print()
    print("=" * 92)
    print(title)
    print("=" * 92)
    print(f"{'workload':{LABEL_W}}  {'oxide':>10}  {'rusty':>10}  {'ratio':>6}  notes")
    print("-" * 92)
    for label, ox, rusty in rows:
        if isinstance(rusty, str):  # rusty raised; keep columns aligned.
            rusty_str, ratio, note = "n/a", "-", rusty.replace("ERR(", "rusty ").rstrip(")")
        else:
            rusty_str = _fmt_us(rusty)
            ratio, note = _ratio_and_flag(ox, rusty)
        print(
            f"{label:{LABEL_W}.{LABEL_W}}  {_fmt_us(ox):>10}  "
            f"{rusty_str:>10}  {ratio:>6}  {note}"
        )


def _print_zones(label, ox, src, ctx, iters):
    tpl = ox.from_string(src)
    tpl.render(ctx)
    reset_prof_stats()
    for _ in range(iters):
        tpl.render(ctx)
    stats = dict(get_prof_stats())
    print(f"\n  {label}  (prof zones, iters={iters})")
    if not stats:
        print("    (prof feature not enabled - rebuild with --features prof)")
        return
    items = sorted(stats.items(), key=lambda kv: -kv[1]["total_us"])
    print(f"    {'zone':46}  {'calls':>6}  {'us/run':>8}  {'ns/call':>8}")
    print(f"    {'-' * 46}  {'-' * 6}  {'-' * 8}  {'-' * 8}")
    for k, s in items[:12]:
        per_run_us = s["total_us"] / iters
        calls = s["count"] // iters
        print(f"    {k:46.46}  {calls:>6}  {per_run_us:>8.2f}  {s['avg_ns']:>8}")


def main():
    filters = sys.argv[1:]
    iters = _render_iters()
    apps = _bench._build_applications(50)
    ctx = {"applications": apps, "empty_apps": []}

    ox = OxideTemplates({"NAME": "ox", **_bench._backend_options()})
    rust = None
    if RustyTemplates is not None:
        ropts = _bench._backend_options()
        ropts["OPTIONS"].pop("loaders", None)
        try:
            rust = RustyTemplates({"NAME": "rust", **ropts})
        except Exception:
            pass

    render = [(lbl, src) for lbl, src in _bench.RENDER_CASES if _matches(lbl, filters)]
    compile_ = [
        (lbl, src) for lbl, src in _bench.COMPILE_CASES if _matches("COMPILE " + lbl, filters)
    ]

    render_rows = []
    for label, src in render:
        ox_ms = _measure(_render_runner(ox, src, ctx), iters)
        rusty_ms = _measure(_render_runner(rust, src, ctx), iters) if rust is not None else None
        render_rows.append((label, ox_ms, rusty_ms))
    _print_table(f"RENDER  (items=50, iters={iters})", render_rows)

    cn = max(100, iters // 10)
    compile_rows = []
    for label, src in compile_:
        ox_ms = _measure(_compile_runner(ox, src), cn)
        rusty_ms = _measure(_compile_runner(rust, src), cn) if rust is not None else None
        compile_rows.append((label, ox_ms, rusty_ms))
    _print_table(f"COMPILE  (iters={cn})", compile_rows)

    # Drill-in: prof zones only for explicitly filtered cases.
    if filters:
        print()
        print("=" * 92)
        print("PROF ZONES (oxide)")
        print("=" * 92)
        for label, src in render:
            _print_zones(label, ox, src, ctx, iters)
    else:
        print("\n(pass case-name substrings as args to see per-zone breakdowns)")


if __name__ == "__main__":
    main()

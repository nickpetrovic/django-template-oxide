# Benchmarks

Three things live here:

- `bench.py`, the comparison bench (oxide vs `django-rusty-templates`
  vs stock Django). What you run to get the headline numbers.
- `perf_drill.py`, a focused micro-profiler for digging into hot
  spots. Used during perf work to find what's slow.
- This README, methodology, how to reproduce, what each workload
  measures.

## Running it

```sh
uv sync --group dev --group bench
uv run --no-sync python benches/bench.py
```

The first run will build `django-rusty-templates` from its upstream
repo (not on PyPI). Subsequent runs reuse the cached wheel.

## What's measured

### Section 1: render workloads

22 distinct cases, each a small Django template rendered against a
synthetic dataset of 50 Application-shaped objects. Each backend
renders the same template the same number of times (default: 200);
we report mean per-render time and the p99 latency tail.

| Case | What it exercises |
|------|-------------------|
| TEXT ONLY | for-loop iteration overhead with no variables |
| VARS ONLY | 3 attribute lookups per row, no filters |
| FULL TEMPLATE | realistic mix: filters, conditionals, 6 columns |
| DEEP LOOKUP | `a.b.c.d.e.f` attribute chain (Variable._resolve_lookup) |
| DICT LOOKUP | 3 dict key reads per row |
| LIST INDEXING | `items.0` integer-keyed lookup |
| FILTER CHAIN | 6-deep filter pipeline (`upper\|lower\|title\|...`) |
| DATE FILTERS | 3 `date` filter invocations per row |
| IF/ELIF CHAIN | 5-branch smartif chain |
| WITH NESTED | 4 nested `{% with %}` blocks |
| FORLOOP COUNTER | `forloop.counter` / `.first` / `.last` access |
| CYCLE TAG | `{% cycle %}` with render_context state |
| AUTOESCAPE HEAVY | HTML metachars in half the rows |
| URL TAG | `{% url 'name' arg %}` reverse per row |
| CSRF TOKEN | `{% csrf_token %}` per row |
| FOR EMPTY | `{% for %}{% empty %}` on an empty list |
| SPACELESS BLOCK | `{% spaceless %}` whitespace stripping |
| CUSTOM PY FILTER | `@register.filter` Python call-out |
| CUSTOM PY simple_tag | `@register.simple_tag` dispatch |
| CUSTOM PY @register.tag | raw `@register.tag` (PyOpaqueNode path) |
| INCLUDE LOOP | `{% include 'fragment.html' %}` in a 50-row loop |
| INHERITANCE | `{% extends %}` + 3 block overrides |

### Section 2: compile time

How fast each engine turns a source string into a compiled template.
Three sizes:

| Size | Rows | Approx node count |
|------|------|--------------------|
| SMALL  | 10  | 120 |
| MEDIUM | 100 | 1200 |
| LARGE  | 500 | 6000 |

This isolates lex+parse from render. Templates are typically
compile-once-render-many in production, so compile time matters
less than render time, but a 30x gap (oxide vs rusty on LARGE) is
worth knowing about.

### Section 3: scaling sweep

The FULL TEMPLATE rendered at items ∈ {1, 10, 100, 1000}. Reports
`ns/item` for the oxide column so you can see per-row cost across
input sizes. Oxide stabilizes at ~1900 ns/item from N=100 upward;
stock Django degrades superlinearly past N=1000.

## Methodology

- **Hardware**: M-series MacBook (whatever the current dev machine
  is). Numbers will differ on Linux x86_64; ratios should not.
- **Warmup**: each case runs once before the timer starts, so JIT,
  module imports, and class lookup caches are warm.
- **Iterations**: 200 by default. Override with `BENCH_ITERS=N`.
- **Reported metric**: mean per-render time in ms. Also the p99
  (single slowest of N renders), that's the parenthetical column
  in the output. A high p99/mean ratio indicates the worst case is
  hitting GC pauses or dict resizes.
- **Error handling**: when a backend can't run a case (rusty bails
  on `WITH`, `CYCLE`, custom tags, etc.), we print `ERROR: <reason>`
  inline so the comparison stays compact.

## Environment knobs

| Variable | Default | What it does |
|----------|---------|--------------|
| `BENCH_ITEMS` | 50 | Synthetic dataset size |
| `BENCH_ITERS` | 200 | Iterations per case |
| `BENCH_SECTIONS` | `render,compile,scaling` | Comma-separated subset |

Examples:

```sh
# Only run compile-time benchmarks
BENCH_SECTIONS=compile uv run --no-sync python benches/bench.py

# Stress test with 5000 rows, 50 iters
BENCH_ITEMS=5000 BENCH_ITERS=50 uv run --no-sync python benches/bench.py
```

## perf_drill.py

A focused micro-profiler for hot-spot work. Targets the cases where
oxide is slowest relative to rusty (the headline cases used to be
FOR EMPTY, FORLOOP COUNTER, COMPILE SMALL, those have since been
optimized). Times each case at 5000 iters for stable numbers, then
dumps the per-zone breakdown from oxide's internal profiler.

Requires building with the `prof` cargo feature:

```sh
uvx maturin develop --release --features=prof
uv run --no-sync python benches/perf_drill.py
```

The `prof` feature has measurable overhead (~5% on hot paths); it's
not enabled in default release builds.

## What the numbers don't tell you

- **Cold start.** First render after process start includes Django
  setup, oxide import, module-cache population. We measure warm
  steady-state.
- **Memory pressure.** Bench renders into pre-allocated buffers
  where possible. A 5 MB output template in a tight memory budget
  will exercise allocators differently.
- **Concurrent rendering.** Single-threaded measurements. Multi-
  threaded throughput depends on Django middleware and ASGI/WSGI
  worker config more than the template engine.
- **Compilation cache hit rate.** The numbers above assume warm
  template cache. Dev mode with autoreload sees more compile
  pressure and oxide's compile advantage matters more.

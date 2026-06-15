# Performance

Numbers from `benches/bench.py`. Smaller is better.

## Render workloads (items=50, iters=200, M-series laptop)

| Workload                       | Oxide    | django-rusty-templates | Stock     |
|--------------------------------|----------|------------------------|-----------|
| TEXT ONLY                      | 0.005ms  | 0.011ms                | 0.019ms   |
| VARS ONLY (3 attrs)            | 0.019ms  | 0.159ms                | 0.296ms   |
| FULL TEMPLATE (real-world)     | 0.104ms  | 0.836ms                | 1.513ms   |
| DEEP LOOKUP (a.b.c.d.e.f)      | 0.027ms  | 0.108ms                | 0.175ms   |
| DICT LOOKUP (3 keys per row)   | 0.025ms  | 0.124ms                | 0.270ms   |
| LIST INDEXING (tags.0)         | 0.009ms  | 0.066ms                | 0.368ms   |
| FILTER CHAIN (6-deep pipeline) | 0.032ms  | unsupported            | 0.684ms   |
| DATE FILTERS (3 formats)       | 0.171ms  | 0.993ms                | 1.620ms   |
| IF/ELIF CHAIN (5 branches)     | 0.022ms  | 0.075ms                | 0.140ms   |
| WITH NESTED (4 levels)         | 0.103ms  | unsupported            | 0.754ms   |
| FORLOOP COUNTER                | 0.022ms  | 0.075ms                | 0.408ms   |
| CYCLE TAG                      | 0.019ms  | unsupported            | 0.148ms   |
| AUTOESCAPE HEAVY               | 0.012ms  | 0.054ms                | 0.112ms   |
| NESTED LOOP (apps x tags)      | 0.061ms  | 0.123ms                | 0.784ms   |
| IF BOOLEAN (and/or/not/in)     | 0.038ms  | 0.070ms                | 0.110ms   |
| I18N TRANSLATE (per row)       | 0.167ms  | unsupported            | 0.384ms   |
| LONG TEXT AUTOESCAPE (prose)   | 0.036ms  | 0.089ms                | 0.174ms   |
| PROSE FILTERS (truncate+breaks)| 0.084ms  | unsupported            | 0.557ms   |
| URL TAG                        | 0.455ms  | 0.543ms                | 0.722ms   |
| CSRF TOKEN                     | 0.005ms  | 0.027ms                | 0.044ms   |
| FOR EMPTY (empty list path)    | 0.001ms  | wrong output           | 0.005ms   |
| SPACELESS BLOCK                | 0.023ms  | unsupported            | 0.198ms   |
| CUSTOM PY FILTER (per row)     | 0.042ms  | unsupported            | 0.123ms   |
| CUSTOM PY simple_tag (per row) | 0.086ms  | unsupported            | 0.120ms   |
| CUSTOM PY @register.tag        | 0.029ms  | unsupported            | 0.063ms   |
| REGROUP (by status)            | 0.121ms  | unsupported            | 0.579ms   |
| FILTER VAR ARG (default:var)   | 0.025ms  | 0.059ms                | 0.165ms   |
| INCLUDE LOOP (50 includes)     | 0.042ms  | unsupported            | 0.457ms   |
| INHERITANCE (extends + blocks) | 0.037ms  | unsupported            | 0.344ms   |
| INHERITANCE 3-LEVEL (block.super)| 0.113ms| unsupported            | 0.395ms   |

## Compile time

| Template size            | Oxide    | django-rusty-templates | Stock    |
|--------------------------|----------|------------------------|----------|
| SMALL (10 rows, 120 nodes)| 0.158ms  | 0.189ms                | 0.915ms  |
| MEDIUM (100 rows, 1.2K nodes)| 1.38ms   | 14.29ms                | 9.52ms   |
| LARGE (500 rows, 6K nodes)| 6.93ms   | 349.05ms               | 49.38ms  |

Rusty's parser is superlinear (0.19ms → 14.29ms → 349.05ms across
10x → 100x → 500x input). Oxide is linear (0.16ms → 1.38ms → 6.93ms),
and now edges out rusty even on the small template.

## Scaling (FULL TEMPLATE across item counts)

| N items | Oxide    | Rusty    | Stock    | Oxide ns/item |
|---------|----------|----------|----------|---------------|
| 1       | 0.004ms  | 0.020ms  | 0.037ms  | 4052          |
| 10      | 0.022ms  | 0.174ms  | 0.326ms  | 2225          |
| 100     | 0.201ms  | 1.678ms  | 3.057ms  | 2011          |
| 1000    | 2.017ms  | 16.996ms | 31.063ms | 2017          |

Oxide stabilizes at ~2000 ns/item from N=100 upward. Stock degrades
superlinearly past N=1000 (p99 climbs faster than mean).

## Reproducing

```sh
uv sync --group dev
uv run --no-sync python benches/bench.py
```

Environment variables:

- `BENCH_ITEMS=N`, synthetic dataset size (default 50)
- `BENCH_ITERS=N`, iterations per case (default 200)
- `BENCH_SECTIONS=render,compile,scaling`, pick which sections to run

See `benches/README.md` for what each workload measures and the
methodology behind the numbers.

# Performance

Numbers from `benches/bench.py`. Smaller is better.

## Render workloads (items=50, iters=200, M-series laptop)

| Workload                       | Oxide    | django-rusty-templates | Stock     |
|--------------------------------|----------|------------------------|-----------|
| TEXT ONLY                      | 0.005ms  | 0.009ms                | 0.017ms   |
| VARS ONLY (3 attrs)            | 0.018ms  | 0.124ms                | 0.276ms   |
| FULL TEMPLATE (real-world)     | 0.100ms  | 0.696ms                | 1.409ms   |
| DEEP LOOKUP (a.b.c.d.e.f)      | 0.032ms  | 0.089ms                | 0.166ms   |
| DICT LOOKUP (3 keys per row)   | 0.026ms  | 0.104ms                | 0.236ms   |
| LIST INDEXING (tags.0)         | 0.009ms  | 0.054ms                | 0.339ms   |
| FILTER CHAIN (6-deep pipeline) | 0.035ms  | unsupported            | 0.626ms   |
| DATE FILTERS (3 formats)       | 0.158ms  | 0.880ms                | 1.464ms   |
| IF/ELIF CHAIN (5 branches)     | 0.024ms  | 0.059ms                | 0.140ms   |
| WITH NESTED (4 levels)         | 0.115ms  | unsupported            | 0.704ms   |
| FORLOOP COUNTER                | 0.026ms  | 0.058ms                | 0.385ms   |
| CYCLE TAG                      | 0.021ms  | unsupported            | 0.137ms   |
| AUTOESCAPE HEAVY               | 0.013ms  | 0.042ms                | 0.098ms   |
| URL TAG                        | 0.465ms  | 0.477ms                | 0.664ms   |
| CSRF TOKEN                     | 0.005ms  | 0.019ms                | 0.036ms   |
| FOR EMPTY (empty list path)    | 0.001ms  | 0.001ms                | 0.005ms   |
| SPACELESS BLOCK                | 0.024ms  | unsupported            | 0.176ms   |
| CUSTOM PY FILTER (per row)     | 0.042ms  | unsupported            | 0.113ms   |
| CUSTOM PY simple_tag (per row) | 0.083ms  | unsupported            | 0.106ms   |
| CUSTOM PY @register.tag        | 0.033ms  | unsupported            | 0.057ms   |
| INCLUDE LOOP (50 includes)     | 0.050ms  | unsupported            | 0.431ms   |
| INHERITANCE (extends + blocks) | 0.068ms  | unsupported            | 0.329ms   |

## Compile time

| Template size            | Oxide    | django-rusty-templates | Stock    |
|--------------------------|----------|------------------------|----------|
| SMALL (10 rows, 120 nodes)| 0.171ms  | 0.190ms                | 0.805ms  |
| MEDIUM (100 rows, 1.2K nodes)| 1.55ms   | 15.79ms                | 8.72ms   |
| LARGE (500 rows, 6K nodes)| 7.85ms   | 390.59ms               | 46.06ms  |

Rusty's parser is superlinear (0.19ms → 15.79ms → 390.59ms across
10x → 100x → 500x input). Oxide is linear (0.17ms → 1.55ms → 7.85ms),
and now edges out rusty even on the small template.

## Scaling (FULL TEMPLATE across item counts)

| N items | Oxide    | Rusty    | Stock    | Oxide ns/item |
|---------|----------|----------|----------|---------------|
| 1       | 0.004ms  | 0.018ms  | 0.034ms  | 3992          |
| 10      | 0.021ms  | 0.143ms  | 0.287ms  | 2109          |
| 100     | 0.188ms  | 1.401ms  | 2.828ms  | 1875          |
| 1000    | 1.894ms  | 14.174ms | 28.558ms | 1894          |

Oxide stabilizes at ~1900 ns/item from N=100 upward. Stock degrades
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

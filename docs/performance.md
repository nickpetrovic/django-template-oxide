# Performance

Numbers from `benches/bench.py`. Smaller is better.

## Render workloads (items=50, iters=200, M-series laptop)

| Workload                       | Oxide    | django-rusty-templates | Stock     |
|--------------------------------|----------|------------------------|-----------|
| TEXT ONLY                      | 0.004ms  | 0.009ms                | 0.016ms   |
| VARS ONLY (3 attrs)            | 0.017ms  | 0.122ms                | 0.264ms   |
| FULL TEMPLATE (real-world)     | 0.096ms  | 0.680ms                | 1.327ms   |
| DEEP LOOKUP (a.b.c.d.e.f)      | 0.032ms  | 0.087ms                | 0.161ms   |
| DICT LOOKUP (3 keys per row)   | 0.026ms  | 0.095ms                | 0.237ms   |
| LIST INDEXING (tags.0)         | 0.008ms  | 0.052ms                | 0.327ms   |
| FILTER CHAIN (6-deep pipeline) | 0.034ms  | unsupported            | 0.609ms   |
| DATE FILTERS (3 formats)       | 0.151ms  | 0.852ms                | 1.442ms   |
| IF/ELIF CHAIN (5 branches)     | 0.021ms  | 0.056ms                | 0.125ms   |
| WITH NESTED (4 levels)         | 0.108ms  | unsupported            | 0.669ms   |
| FORLOOP COUNTER                | 0.026ms  | 0.055ms                | 0.361ms   |
| CYCLE TAG                      | 0.019ms  | unsupported            | 0.133ms   |
| AUTOESCAPE HEAVY               | 0.012ms  | 0.042ms                | 0.097ms   |
| URL TAG                        | 0.435ms  | 0.472ms                | 0.638ms   |
| CSRF TOKEN                     | 0.004ms  | 0.017ms                | 0.036ms   |
| INCLUDE LOOP                   | 0.033ms  | unsupported            | 0.404ms   |
| INHERITANCE (extends + blocks) | 0.016ms  | unsupported            | 0.303ms   |

## Compile time

| Template size            | Oxide    | django-rusty-templates | Stock    |
|--------------------------|----------|------------------------|----------|
| SMALL (10 rows, 120 nodes)| 0.252ms  | 0.186ms                | 0.805ms  |
| MEDIUM (100 rows, 1.2K nodes)| 2.39ms   | 15.51ms                | 8.29ms   |
| LARGE (500 rows, 6K nodes)| 12.10ms  | 380ms                  | 44ms     |

Rusty's parser is superlinear (0.19ms → 15.5ms → 381ms across
10x → 100x → 500x input). Oxide is linear (0.25ms → 2.4ms → 12ms).

## Scaling (FULL TEMPLATE across item counts)

| N items | Oxide    | Rusty    | Stock    | Oxide ns/item |
|---------|----------|----------|----------|---------------|
| 1       | 0.003ms  | 0.016ms  | 0.034ms  | 3271          |
| 10      | 0.020ms  | 0.137ms  | 0.272ms  | 1974          |
| 100     | 0.187ms  | 1.366ms  | 2.672ms  | 1871          |
| 1000    | 1.877ms  | 14.16ms  | 27.44ms  | 1877          |

Oxide stabilizes at ~1900 ns/item from N=100 upward. Stock degrades
superlinearly past N=1000 (p99 climbs faster than mean).

## Reproducing

```sh
uv sync --group dev --group bench
uv run --no-sync python benches/bench.py
```

Environment variables:

- `BENCH_ITEMS=N`, synthetic dataset size (default 50)
- `BENCH_ITERS=N`, iterations per case (default 200)
- `BENCH_SECTIONS=render,compile,scaling`, pick which sections to run

See `benches/README.md` for what each workload measures and the
methodology behind the numbers.

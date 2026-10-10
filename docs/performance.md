# Performance

Numbers from `benches/bench.py`: mean time per render or compile, with
the same template and data for each engine. Smaller is better.

## Test machine

| Item                   | Value                                               |
|------------------------|-----------------------------------------------------|
| Computer               | MacBook Pro (Mac17,6)                               |
| Chip                   | Apple M5 Max, 18 cores (6 Super, 12 Performance)    |
| Memory                 | 64 GB                                               |
| Operating system       | macOS 27.0.1                                        |
| Python                 | CPython 3.14.7, free-threaded build                 |
| Django                 | 6.0.5                                               |
| Rust                   | 1.96.1                                              |
| django-template-oxide  | commit `888e034`                                    |
| django-rusty-templates | 0.1.0, commit `5adbe61`                             |

Run on October 9, 2026, with the default settings (50 rows, 200
iterations per render case, 40 per compile case). Repeated runs on a
busier machine came out up to about 15% slower for every engine; the
ratios between engines stayed the same.

## Render workloads

| Workload                          | Oxide    | django-rusty-templates | Stock    |
|-----------------------------------|----------|------------------------|----------|
| TEXT ONLY                         | 0.003ms  | 0.003ms                | 0.010ms  |
| VARS ONLY (3 attrs)               | 0.010ms  | 0.063ms                | 0.161ms  |
| FULL TEMPLATE (real-world)        | 0.044ms  | 0.381ms                | 0.805ms  |
| DEEP LOOKUP (a.b.c.d.e.f)         | 0.016ms  | 0.043ms                | 0.095ms  |
| DICT LOOKUP (3 keys per row)      | 0.017ms  | 0.049ms                | 0.144ms  |
| LIST INDEXING (tags.0)            | 0.006ms  | 0.024ms                | 0.194ms  |
| FILTER CHAIN (6-deep pipeline)    | 0.016ms  | syntax error           | 0.357ms  |
| DATE FILTERS (3 formats)          | 0.058ms  | 0.488ms                | 0.854ms  |
| IF/ELIF CHAIN (5 branches)        | 0.015ms  | 0.025ms                | 0.076ms  |
| WITH NESTED (4 levels)            | 0.062ms  | unsupported            | 0.399ms  |
| FORLOOP COUNTER                   | 0.015ms  | 0.029ms                | 0.231ms  |
| CYCLE TAG                         | 0.011ms  | 0.028ms                | 0.082ms  |
| AUTOESCAPE HEAVY                  | 0.008ms  | 0.023ms                | 0.059ms  |
| NESTED LOOP (apps x tags)         | 0.035ms  | 0.044ms                | 0.435ms  |
| IF BOOLEAN (and/or/not/in)        | 0.013ms  | 0.026ms                | 0.062ms  |
| I18N TRANSLATE (per row)          | 0.095ms  | unsupported            | 0.212ms  |
| LONG TEXT AUTOESCAPE (prose)      | 0.023ms  | 0.036ms                | 0.079ms  |
| PROSE FILTERS (truncate+breaks)   | 0.050ms  | syntax error           | 0.273ms  |
| URL TAG                           | 0.251ms  | 0.281ms                | 0.391ms  |
| CSRF TOKEN                        | 0.003ms  | 0.010ms                | 0.022ms  |
| FOR EMPTY (empty list path)       | <0.001ms | 0.001ms                | 0.003ms  |
| SPACELESS BLOCK                   | 0.013ms  | unsupported            | 0.100ms  |
| CUSTOM PY FILTER (per row)        | 0.010ms  | unsupported            | 0.065ms  |
| CUSTOM PY simple_tag (per row)    | 0.046ms  | unsupported            | 0.063ms  |
| CUSTOM PY @register.tag           | 0.018ms  | unsupported            | 0.035ms  |
| REGROUP (by status)               | 0.044ms  | unsupported            | 0.314ms  |
| FILTER VAR ARG (default:var)      | 0.009ms  | 0.022ms                | 0.088ms  |
| INCLUDE LOOP (50 includes)        | 0.026ms  | unsupported            | 0.244ms  |
| INHERITANCE (extends + blocks)    | 0.021ms  | unsupported            | 0.187ms  |
| INHERITANCE 3-LEVEL (block.super) | 0.061ms  | unsupported            | 0.217ms  |

"Unsupported" means rusty raised an error saying the tag or template
loading is not implemented. "Syntax error" means rusty rejected
template syntax that Django accepts.

## Compile time

| Template size                 | Oxide    | django-rusty-templates | Stock    |
|-------------------------------|----------|------------------------|----------|
| SMALL (10 rows, 120 nodes)    | 0.087ms  | 0.088ms                | 0.454ms  |
| MEDIUM (100 rows, 1.2K nodes) | 0.767ms  | 6.927ms                | 4.474ms  |
| LARGE (500 rows, 6K nodes)    | 3.738ms  | 166.976ms              | 23.038ms |

Oxide's compile time grows in proportion to template size (0.087ms,
0.767ms, 3.738ms). Rusty's grows faster than the template does
(0.088ms, 6.927ms, 166.976ms). On the small template the two are
tied; see [Limitations](limitations.md) for why.

## Scaling (FULL TEMPLATE across row counts)

| Rows | Oxide   | Rusty   | Stock    | Oxide ns per row |
|------|---------|---------|----------|------------------|
| 1    | 0.005ms | 0.009ms | 0.020ms  | 5415             |
| 10   | 0.012ms | 0.078ms | 0.167ms  | 1164             |
| 100  | 0.080ms | 0.749ms | 1.609ms  | 801              |
| 1000 | 0.783ms | 7.528ms | 16.196ms | 783              |

Oxide settles at about 800 ns per row from 100 rows upward.

## Context entry (FULL TEMPLATE, 50 rows)

| How the context is passed | Oxide   | Rusty                  | Stock   |
|---------------------------|---------|------------------------|---------|
| Plain dict                | 0.043ms | 0.375ms                | 0.810ms |
| `Context` object          | 0.043ms | error (AttributeError) | 0.807ms |
| Dict with 200 extra keys  | 0.049ms | 0.393ms                | 0.808ms |

## About django-rusty-templates

[django-rusty-templates](https://github.com/LilyFirefly/django-rusty-templates)
is an earlier Rust-backed Django template engine (repository created
September 2024). Its README describes it as experimental and "not yet
ready for full release". In the commit benchmarked here it raises
`NotImplementedError` for several common tags (`{% with %}`,
`{% spaceless %}`, custom `@register.tag`) and for `{% include %}` and
`{% extends %}` template loading.

Oxide took the other approach: match Django completely now, including
third-party hooks like django-cotton's `Lexer.tokenize` patch, even
where that means calling back into Python where rusty stays in Rust.
When rusty reaches full compatibility the comparison will be more
meaningful. Until then, the benchmark runs both engines and reports
where rusty fails.

## Reproducing

```sh
uv sync --group dev
uvx maturin develop --release
uv run --no-sync python benches/bench.py
```

Environment variables:

- `BENCH_ITEMS=N`: number of rows in the synthetic dataset (default 50)
- `BENCH_ITERS=N`: iterations per render case (default 200)
- `BENCH_SECTIONS=render,compile,scaling,context`: which sections to run

See `benches/README.md` for what each workload measures and how the
numbers are collected.

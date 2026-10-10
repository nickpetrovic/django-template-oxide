# Performance

Numbers from `benches/bench.py`, which renders the same templates with
the same data in oxide, django-rusty-templates ("rusty"), and stock
Django. Every time is per render or per compile, so smaller is better.
The "vs" columns show how many times faster oxide is than the other
engine.

## Test machine

| Item                   | Value                                            |
|------------------------|--------------------------------------------------|
| Computer               | MacBook Pro (Mac17,6)                            |
| Chip                   | Apple M5 Max, 18 cores (6 Super, 12 Performance) |
| Memory                 | 64 GB                                            |
| Operating system       | macOS 27.0.1                                     |
| Python, regular build  | CPython 3.14.6                                   |
| Python, free-threaded  | CPython 3.14.7 (3.14t), GIL off                  |
| Django                 | 6.1.2                                            |
| Rust                   | 1.96.1                                           |
| django-template-oxide  | 0.1.1                                            |
| django-rusty-templates | 0.1.0, commit `5adbe61`                          |

Run on October 9, 2026, with the default settings: 50 rows of data,
and each time is the median of 9 samples of at least 10 ms each, with
the engines taking turns sample by sample. Every engine's output was
checked against stock Django before it was timed.

The tables below come from the regular build. The free-threaded build
gave results within a few percent of these for every workload, so only
its thread and memory results are shown separately.

## Summary

| Comparison            | Regular build | Free-threaded build | Workloads compared | Oxide slower on |
|-----------------------|--------------:|--------------------:|-------------------:|-----------------|
| Oxide vs rusty        | 3.4× faster   | 3.3× faster         | 32 of 50           | none            |
| Oxide vs stock Django | 6.4× faster   | 6.4× faster         | 50 of 50           | none            |

The speedup is the geometric mean over the workloads both engines can
run. Rusty could not run 18 of the 50 workloads; the tables show why
for each one.

## Template features

Each workload renders one template over 50 rows of plain Python
objects.

| Workload                                       | Oxide    | Rusty         | Stock Django | vs rusty | vs stock |
|------------------------------------------------|---------:|--------------:|-------------:|---------:|---------:|
| Text only, no variables                        | 2.17 µs  | 2.93 µs       | 9.62 µs      | 1.3×     | 4.4×     |
| Three attribute lookups                        | 8.95 µs  | 65.4 µs       | 159.8 µs     | 7.3×     | 18×      |
| Full table (mixed tags and filters)            | 35.4 µs  | 368.2 µs      | 758.1 µs     | 10×      | 21×      |
| Deep lookup (six levels)                       | 11.8 µs  | 41.0 µs       | 87.0 µs      | 3.5×     | 7.4×     |
| Dictionary lookups                             | 14.2 µs  | 52.0 µs       | 143.7 µs     | 3.7×     | 10×      |
| List indexing                                  | 4.93 µs  | 26.5 µs       | 195.4 µs     | 5.4×     | 40×      |
| Filter chain (six filters)                     | 15.7 µs  | syntax error  | 333.6 µs     |          | 21×      |
| Date filters                                   | 34.7 µs  | 454.7 µs      | 717.2 µs     | 13×      | 21×      |
| `if` / `elif` chain                            | 14.6 µs  | 26.0 µs       | 75.4 µs      | 1.8×     | 5.2×     |
| Nested `{% with %}`                            | 59.1 µs  | not supported | 407.1 µs     |          | 6.9×     |
| `forloop` variables                            | 14.0 µs  | 29.7 µs       | 195.2 µs     | 2.1×     | 14×      |
| `{% cycle %}`                                  | 10.3 µs  | 29.8 µs       | 84.8 µs      | 2.9×     | 8.3×     |
| Autoescaping HTML                              | 7.07 µs  | 22.4 µs       | 60.5 µs      | 3.2×     | 8.6×     |
| Nested loops                                   | 34.5 µs  | 46.0 µs       | 372.9 µs     | 1.3×     | 11×      |
| Boolean `{% if %}` (and, or, not, in)          | 12.2 µs  | 27.1 µs       | 61.6 µs      | 2.2×     | 5.1×     |
| `{% translate %}` per row                      | 66.5 µs  | not supported | 187.4 µs     |          | 2.8×     |
| Long text autoescaping                         | 22.6 µs  | 39.7 µs       | 82.1 µs      | 1.8×     | 3.6×     |
| Text filters (truncatewords, linebreaksbr)     | 51.4 µs  | syntax error  | 284.7 µs     |          | 5.5×     |
| `{% url %}` per row                            | 217.3 µs | 248.3 µs      | 360.1 µs     | 1.1×     | 1.7×     |
| `{% csrf_token %}` per row                     | 2.53 µs  | 10.2 µs       | 22.5 µs      | 4.0×     | 8.9×     |
| Empty loop (`{% empty %}`)                     | 368 ns   | 485 ns        | 2.47 µs      | 1.3×     | 6.7×     |
| `{% spaceless %}`                              | 12.1 µs  | not supported | 104.6 µs     |          | 8.6×     |
| Custom Python filter                           | 9.93 µs  | not supported | 65.4 µs      |          | 6.6×     |
| Custom `simple_tag`                            | 47.3 µs  | not supported | 64.0 µs      |          | 1.4×     |
| Custom `@register.tag`                         | 17.9 µs  | not supported | 33.6 µs      |          | 1.9×     |
| `{% regroup %}`                                | 43.5 µs  | not supported | 281.8 µs     |          | 6.5×     |
| Filter with a variable argument                | 8.33 µs  | 24.7 µs       | 87.6 µs      | 3.0×     | 11×      |
| `{% include %}` per row                        | 24.9 µs  | 45.1 µs       | 226.9 µs     | 1.8×     | 9.1×     |
| `{% extends %}` with three blocks              | 10.8 µs  | not supported | 122.5 µs     |          | 11×      |
| Three-level `{% extends %}` with `block.super` | 12.1 µs  | not supported | 127.5 µs     |          | 11×      |

"Not supported" means rusty raised an error because it does not
implement the feature. "Syntax error" means rusty rejected template
syntax that Django accepts, such as the `truncatechars` and
`truncatewords` filters.

Workloads that call back into Python for each row, such as
`{% url %}` and a custom `simple_tag`, gain the least, because most of
their time is spent in the Python code they call.

## Django objects

These workloads render real Django objects from an in-memory SQLite
database instead of plain Python objects.

| Workload                                  | Oxide    | Rusty        | Stock Django | vs rusty | vs stock |
|-------------------------------------------|---------:|-------------:|-------------:|---------:|---------:|
| Model instances (User fields and methods) | 375.7 µs | wrong output | 906.9 µs     |          | 2.4×     |
| Foreign keys (Permission to ContentType)  | 136.4 µs | 189.3 µs     | 298.0 µs     | 1.4×     | 2.2×     |
| QuerySet queried during render            | 208.3 µs | 220.3 µs     | 272.1 µs     | 1.06×    | 1.3×     |
| Lazy translation strings                  | 159.9 µs | 170.8 µs     | 285.6 µs     | 1.07×    | 1.8×     |
| Form fields with errors                   | 305.6 µs | 319.1 µs     | 329.0 µs     | 1.04×    | 1.08×    |

"Wrong output" means rusty rendered the template but the result
differed from stock Django's. The gap between engines narrows here
because these renders spend most of their time in Django itself:
running the query, evaluating lazy strings, and rendering form widgets.

## Whole pages

Each page is loaded with `get_template` and rendered with a request,
the way a view renders it.

| Workload                                   | Oxide    | Rusty         | Stock Django | vs stock |
|--------------------------------------------|---------:|--------------:|-------------:|---------:|
| Django admin login page                    | 357.2 µs | not supported | 419.4 µs     | 1.2×     |
| Django admin index page                    | 423.6 µs | not supported | 536.5 µs     | 1.3×     |
| django-cotton page (layout, cards, badges) | 930.6 µs | wrong output  | 1.13 ms      | 1.2×     |

Most of an admin page's time goes to context processors, URL
reversing, and form rendering in Python, which a template engine
cannot speed up.

## Compiling templates

Compiling from source, with no cache.

| Template size                        | Oxide    | Rusty    | Stock Django | vs rusty | vs stock |
|--------------------------------------|---------:|---------:|-------------:|---------:|---------:|
| Small (10 rows, about 120 nodes)     | 90.1 µs  | 91.0 µs  | 487.8 µs     | same     | 5.4×     |
| Medium (100 rows, about 1,200 nodes) | 811.0 µs | 7.20 ms  | 4.88 ms      | 8.9×     | 6.0×     |
| Large (500 rows, about 6,000 nodes)  | 4.08 ms  | 174.1 ms | 25.4 ms      | 43×      | 6.2×     |

Oxide's compile time grows in proportion to template size. Rusty's
grows faster than the template does. On the small template the two are
tied; see [Limitations](limitations.md) for why.

## Loading templates

`get_template` followed by a render, the way views use templates.

| Loader                         | Oxide   | Rusty         | Stock Django | vs stock |
|--------------------------------|--------:|--------------:|-------------:|---------:|
| Cached loader (production)     | 11.8 µs | not supported | 125.5 µs     | 11×      |
| No cache (compiles every time) | 93.8 µs | not supported | 268.5 µs     | 2.9×     |

## Scaling with data size

The full table template at different row counts.

| Rows  | Oxide    | Rusty    | Stock Django | vs rusty | vs stock |
|------:|---------:|---------:|-------------:|---------:|---------:|
| 1     | 2.03 µs  | 8.14 µs  | 18.6 µs      | 4.0×     | 9.2×     |
| 10    | 8.29 µs  | 75.5 µs  | 156.3 µs     | 9.1×     | 19×      |
| 100   | 69.8 µs  | 738.3 µs | 1.53 ms      | 11×      | 22×      |
| 1,000 | 685.3 µs | 7.38 ms  | 15.3 ms      | 11×      | 22×      |

Oxide settles at about 690 ns per row from 100 rows upward.

## Passing the context

The full table template with 50 rows, with the context passed in
different ways.

| How the context is passed | Oxide   | Rusty         | Stock Django | vs rusty | vs stock |
|---------------------------|--------:|--------------:|-------------:|---------:|---------:|
| Plain dict                | 35.7 µs | 370.2 µs      | 767.1 µs     | 10×      | 21×      |
| Django `Context` object   | 35.7 µs | not supported | 752.3 µs     |          | 21×      |
| Dict with 200 extra keys  | 41.7 µs | 391.4 µs      | 775.2 µs     | 9.4×     | 19×      |

## Rendering from several threads

Renders per second of the full table template (50 rows). The threads
share one compiled template and each renders its own data, the way a
web server's threads share cached templates but build a new context
for every request. Larger is better. The number in brackets is the
throughput relative to one thread.

Regular build (GIL on), where threads take turns and throughput is
expected to stay flat:

| Threads | Oxide           | Rusty          | Stock Django   |
|--------:|----------------:|---------------:|---------------:|
| 1       | 26,700/s (1.0×) | 2,501/s (1.0×) | 1,196/s (1.0×) |
| 2       | 27,243/s (1.0×) | 2,487/s (1.0×) | 1,207/s (1.0×) |
| 4       | 26,866/s (1.0×) | 2,507/s (1.0×) | 1,198/s (1.0×) |
| 8       | 26,656/s (1.0×) | 2,496/s (1.0×) | 1,189/s (1.0×) |

Free-threaded build (GIL off):

| Threads | Oxide            | Rusty          | Stock Django   |
|--------:|-----------------:|---------------:|---------------:|
| 1       | 26,411/s (1.0×)  | 2,537/s (1.0×) | 1,170/s (1.0×) |
| 2       | 48,021/s (1.8×)  | 2,714/s (1.1×) | 1,680/s (1.4×) |
| 4       | 91,492/s (3.5×)  | 3,817/s (1.5×) | 2,985/s (2.6×) |
| 8       | 129,691/s (4.9×) | 2,659/s (1.0×) | 2,138/s (1.8×) |

On the free-threaded build, oxide renders about five times as many
pages per second with eight threads as with one. See
[Limitations](limitations.md) for what still keeps it below eight
times.

## Memory

Extra peak memory (resident set size) of a fresh process, including
memory allocated in Rust: first while compiling the large template,
then while rendering the full table template with 1,000 rows twenty
times.

| Engine       | Compile, regular | Render, regular | Compile, free-threaded | Render, free-threaded |
|--------------|-----------------:|----------------:|-----------------------:|----------------------:|
| Oxide        | 6.1 MB           | 0.3 MB          | 6.3 MB                 | 0.6 MB                |
| Rusty        | 2.1 MB           | 0.7 MB          | 1.9 MB                 | 0.9 MB                |
| Stock Django | 4.9 MB           | 1.0 MB          | 4.9 MB                 | 2.9 MB                |

Oxide uses the most memory to compile a large template and the least
to render one.

## About django-rusty-templates

[django-rusty-templates](https://github.com/LilyFirefly/django-rusty-templates)
is an earlier Rust-backed Django template engine (repository created
September 2024). Its README describes it as experimental and "not yet
ready for full release". In the commit benchmarked here it raises
`NotImplementedError` for several common tags (`{% with %}`,
`{% spaceless %}`, `{% regroup %}`, and custom tags) and for
`{% extends %}`, which every Django admin page uses. It also cannot
render a Django `Context` object.

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

A full run takes about a minute, and `--quick` takes about 20 seconds.
`--json results.json` saves every sample, and
`bench.py compare before.json after.json` compares two saved runs.
[`benches/README.md`](https://github.com/nickpetrovic/django-template-oxide/blob/main/benches/README.md)
covers every option, what each workload measures, how the numbers are
collected, and how to measure the regular and free-threaded builds
side by side.

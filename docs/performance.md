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
| django-template-oxide  | 0.1.0, commit `f827062`                          |
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
| Oxide vs rusty        | 3.2× faster   | 3.1× faster         | 32 of 50           | none            |
| Oxide vs stock Django | 6.2× faster   | 6.1× faster         | 50 of 50           | none            |

The speedup is the geometric mean over the workloads both engines can
run. Rusty could not run 18 of the 50 workloads; the tables show why
for each one.

## Template features

Each workload renders one template over 50 rows of plain Python
objects.

| Workload                                       | Oxide    | Rusty         | Stock Django | vs rusty | vs stock |
|------------------------------------------------|---------:|--------------:|-------------:|---------:|---------:|
| Text only, no variables                        | 2.03 µs  | 2.87 µs       | 9.61 µs      | 1.4×     | 4.7×     |
| Three attribute lookups                        | 8.99 µs  | 63.7 µs       | 161.8 µs     | 7.1×     | 18×      |
| Full table (mixed tags and filters)            | 41.4 µs  | 359.6 µs      | 760.1 µs     | 8.7×     | 18×      |
| Deep lookup (six levels)                       | 11.9 µs  | 39.6 µs       | 87.4 µs      | 3.3×     | 7.3×     |
| Dictionary lookups                             | 14.4 µs  | 49.0 µs       | 143.9 µs     | 3.4×     | 10×      |
| List indexing                                  | 5.58 µs  | 25.7 µs       | 191.2 µs     | 4.6×     | 34×      |
| Filter chain (six filters)                     | 15.7 µs  | syntax error  | 332.7 µs     |          | 21×      |
| Date filters                                   | 52.0 µs  | 450.5 µs      | 712.9 µs     | 8.7×     | 14×      |
| `if` / `elif` chain                            | 14.7 µs  | 26.3 µs       | 75.2 µs      | 1.8×     | 5.1×     |
| Nested `{% with %}`                            | 58.6 µs  | not supported | 406.2 µs     |          | 6.9×     |
| `forloop` variables                            | 14.4 µs  | 29.0 µs       | 192.9 µs     | 2.0×     | 13×      |
| `{% cycle %}`                                  | 10.4 µs  | 29.2 µs       | 82.8 µs      | 2.8×     | 7.9×     |
| Autoescaping HTML                              | 7.15 µs  | 21.8 µs       | 60.5 µs      | 3.0×     | 8.5×     |
| Nested loops                                   | 34.8 µs  | 44.5 µs       | 365.9 µs     | 1.3×     | 11×      |
| Boolean `{% if %}` (and, or, not, in)          | 12.2 µs  | 26.9 µs       | 60.3 µs      | 2.2×     | 4.9×     |
| `{% translate %}` per row                      | 68.1 µs  | not supported | 186.9 µs     |          | 2.7×     |
| Long text autoescaping                         | 22.0 µs  | 39.2 µs       | 81.1 µs      | 1.8×     | 3.7×     |
| Text filters (truncatewords, linebreaksbr)     | 50.8 µs  | syntax error  | 281.7 µs     |          | 5.5×     |
| `{% url %}` per row                            | 212.9 µs | 246.3 µs      | 354.4 µs     | 1.2×     | 1.7×     |
| `{% csrf_token %}` per row                     | 2.58 µs  | 9.88 µs       | 22.4 µs      | 3.8×     | 8.7×     |
| Empty loop (`{% empty %}`)                     | 362 ns   | 484 ns        | 2.42 µs      | 1.3×     | 6.7×     |
| `{% spaceless %}`                              | 11.7 µs  | not supported | 102.8 µs     |          | 8.8×     |
| Custom Python filter                           | 10.0 µs  | not supported | 64.6 µs      |          | 6.5×     |
| Custom `simple_tag`                            | 47.2 µs  | not supported | 63.0 µs      |          | 1.3×     |
| Custom `@register.tag`                         | 18.2 µs  | not supported | 33.4 µs      |          | 1.8×     |
| `{% regroup %}`                                | 44.5 µs  | not supported | 279.2 µs     |          | 6.3×     |
| Filter with a variable argument                | 8.54 µs  | 23.5 µs       | 87.6 µs      | 2.8×     | 10×      |
| `{% include %}` per row                        | 24.1 µs  | 44.4 µs       | 222.0 µs     | 1.8×     | 9.2×     |
| `{% extends %}` with three blocks              | 10.5 µs  | not supported | 120.9 µs     |          | 11×      |
| Three-level `{% extends %}` with `block.super` | 11.8 µs  | not supported | 127.2 µs     |          | 11×      |

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
| Model instances (User fields and methods) | 384.2 µs | wrong output | 880.5 µs     |          | 2.3×     |
| Foreign keys (Permission to ContentType)  | 135.0 µs | 183.7 µs     | 295.4 µs     | 1.4×     | 2.2×     |
| QuerySet queried during render            | 205.8 µs | 220.4 µs     | 269.8 µs     | 1.07×    | 1.3×     |
| Lazy translation strings                  | 157.7 µs | 166.9 µs     | 281.6 µs     | 1.06×    | 1.8×     |
| Form fields with errors                   | 295.3 µs | 306.3 µs     | 316.8 µs     | 1.04×    | 1.07×    |

"Wrong output" means rusty rendered the template but the result
differed from stock Django's. The gap between engines narrows here
because these renders spend most of their time in Django itself:
running the query, evaluating lazy strings, and rendering form widgets.

## Whole pages

Each page is loaded with `get_template` and rendered with a request,
the way a view renders it.

| Workload                                   | Oxide    | Rusty         | Stock Django | vs stock |
|--------------------------------------------|---------:|--------------:|-------------:|---------:|
| Django admin login page                    | 349.2 µs | not supported | 407.5 µs     | 1.2×     |
| Django admin index page                    | 410.0 µs | not supported | 529.7 µs     | 1.3×     |
| django-cotton page (layout, cards, badges) | 922.3 µs | wrong output  | 1.12 ms      | 1.2×     |

Most of an admin page's time goes to context processors, URL
reversing, and form rendering in Python, which a template engine
cannot speed up.

## Compiling templates

Compiling from source, with no cache.

| Template size                        | Oxide    | Rusty    | Stock Django | vs rusty | vs stock |
|--------------------------------------|---------:|---------:|-------------:|---------:|---------:|
| Small (10 rows, about 120 nodes)     | 88.1 µs  | 88.9 µs  | 481.9 µs     | same     | 5.5×     |
| Medium (100 rows, about 1,200 nodes) | 807.5 µs | 7.04 ms  | 4.85 ms      | 8.7×     | 6.0×     |
| Large (500 rows, about 6,000 nodes)  | 4.01 ms  | 169.5 ms | 24.7 ms      | 42×      | 6.2×     |

Oxide's compile time grows in proportion to template size. Rusty's
grows faster than the template does. On the small template the two are
tied; see [Limitations](limitations.md) for why.

## Loading templates

`get_template` followed by a render, the way views use templates.

| Loader                         | Oxide   | Rusty         | Stock Django | vs stock |
|--------------------------------|--------:|--------------:|-------------:|---------:|
| Cached loader (production)     | 11.8 µs | not supported | 124.3 µs     | 11×      |
| No cache (compiles every time) | 93.7 µs | not supported | 267.8 µs     | 2.9×     |

## Scaling with data size

The full table template at different row counts.

| Rows  | Oxide    | Rusty    | Stock Django | vs rusty | vs stock |
|------:|---------:|---------:|-------------:|---------:|---------:|
| 1     | 2.65 µs  | 7.96 µs  | 18.5 µs      | 3.0×     | 7.0×     |
| 10    | 9.84 µs  | 72.5 µs  | 152.4 µs     | 7.4×     | 15×      |
| 100   | 79.3 µs  | 726.9 µs | 1.49 ms      | 9.2×     | 19×      |
| 1,000 | 770.8 µs | 7.18 ms  | 15.0 ms      | 9.3×     | 20×      |

Oxide settles at about 770 ns per row from 100 rows upward.

## Passing the context

The full table template with 50 rows, with the context passed in
different ways.

| How the context is passed | Oxide   | Rusty         | Stock Django | vs rusty | vs stock |
|---------------------------|--------:|--------------:|-------------:|---------:|---------:|
| Plain dict                | 40.8 µs | 361.9 µs      | 749.2 µs     | 8.9×     | 18×      |
| Django `Context` object   | 41.3 µs | not supported | 749.3 µs     |          | 18×      |
| Dict with 200 extra keys  | 46.7 µs | 379.1 µs      | 750.8 µs     | 8.1×     | 16×      |

## Rendering from several threads

Renders per second of the full table template (50 rows), with every
thread sharing one compiled template. Larger is better. The number in
brackets is the throughput relative to one thread.

Regular build (GIL on), where threads take turns and throughput is
expected to stay flat:

| Threads | Oxide           | Rusty          | Stock Django   |
|--------:|----------------:|---------------:|---------------:|
| 1       | 23,110/s (1.0×) | 2,569/s (1.0×) | 1,219/s (1.0×) |
| 2       | 22,861/s (1.0×) | 2,564/s (1.0×) | 1,225/s (1.0×) |
| 4       | 22,712/s (1.0×) | 2,569/s (1.0×) | 1,225/s (1.0×) |
| 8       | 22,775/s (1.0×) | 2,560/s (1.0×) | 1,232/s (1.0×) |

Free-threaded build (GIL off):

| Threads | Oxide           | Rusty          | Stock Django   |
|--------:|----------------:|---------------:|---------------:|
| 1       | 22,097/s (1.0×) | 2,528/s (1.0×) | 1,159/s (1.0×) |
| 2       | 22,004/s (1.0×) | 2,736/s (1.1×) | 1,624/s (1.4×) |
| 4       | 30,315/s (1.4×) | 3,858/s (1.5×) | 2,843/s (2.5×) |
| 8       | 21,813/s (1.0×) | 2,604/s (1.0×) | 2,159/s (1.9×) |

On the free-threaded build, oxide does not yet get much faster with
more threads: it peaks at 1.4× with four threads and drops back to the
single-thread rate with eight. Stock Django gains more from extra
threads, but with eight threads it still renders about a tenth as many
pages per second as oxide does with one. See
[Limitations](limitations.md).

## Memory

Extra peak memory (resident set size) of a fresh process, including
memory allocated in Rust: first while compiling the large template,
then while rendering the full table template with 1,000 rows twenty
times.

| Engine       | Compile, regular | Render, regular | Compile, free-threaded | Render, free-threaded |
|--------------|-----------------:|----------------:|-----------------------:|----------------------:|
| Oxide        | 6.2 MB           | 0.2 MB          | 6.3 MB                 | 0.6 MB                |
| Rusty        | 2.1 MB           | 0.7 MB          | 1.9 MB                 | 1.0 MB                |
| Stock Django | 4.9 MB           | 1.0 MB          | 5.3 MB                 | 2.3 MB                |

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

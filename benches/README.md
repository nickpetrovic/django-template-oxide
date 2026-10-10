# Benchmarks

`bench.py` compares oxide with `django-rusty-templates` ("rusty") and
stock Django on the same templates and data, checks that each engine's
output matches stock Django, and prints the results as tables with a
plain-language summary.

## Running it

```sh
uv sync --group dev
uvx maturin develop --release
uv run --no-sync python benches/bench.py
```

`django-rusty-templates` and `django-cotton` are in the `dev` group. The
first `uv sync` builds rusty from its repository, since it is not on
PyPI.

A full run takes about a minute. Useful options:

| Option | What it does |
|--------|--------------|
| `--quick` | Fewer and shorter samples, for a fast look (about 20 seconds) |
| `--sections render,compile` | Run only some sections (see the list below) |
| `--only "date"` | Run only workloads whose name contains the text; repeatable |
| `--items 200` | Rows of data per template (default 50) |
| `--json results.json` | Also save every sample to a JSON file |
| `--profile` | Show where oxide spends time in each workload (needs a profiler build) |
| `--no-color` | Plain output, for logs |

## Comparing two runs

Save a run before and after a change, then compare them:

```sh
uv run --no-sync python benches/bench.py --json before.json
# make the change, rebuild
uv run --no-sync python benches/bench.py --json after.json
uv run --no-sync python benches/bench.py compare before.json after.json
```

`compare` lists oxide's time for each workload in both runs, marks each
one faster, slower, or the same (within measurement noise), and exits
with status 1 if any workload got slower. It warns when the two runs
come from different machines.

## Profiling

`--profile` adds a table after each workload showing oxide's internal
timing zones. It needs oxide built with the `prof` feature, which adds
some overhead, so profiler builds should not be used for published
numbers:

```sh
VIRTUAL_ENV=.venv uvx maturin develop --release --features prof
uv run --no-sync python benches/bench.py --profile --sections render --only "full table"
uvx maturin develop --release
```

## Sections

| Section | What it measures |
|---------|------------------|
| `render` | 30 template features (lookups, filters, tags, includes, inheritance, custom tags) over 50 rows of plain Python objects |
| `django` | Django model instances, foreign keys, a QuerySet queried during the render, lazy translation strings, and a form with errors |
| `pages` | Whole pages through `get_template` and a request: the Django admin login and index pages, and a django-cotton page |
| `compile` | Compiling templates of 120, 1,200, and 6,000 nodes from source, with no cache |
| `loading` | `get_template` plus render the way views do it, with the cached loader and with no cache |
| `scaling` | The full table template at 1, 10, 100, and 1,000 rows |
| `context` | Passing a plain dict, a Django `Context` object, and a dict with 200 extra keys |
| `threads` | Renders per second from 1, 2, 4, and 8 threads sharing one compiled template |
| `memory` | Extra peak memory of a fresh process while compiling the large template and rendering 1,000 rows |

The django-cotton page and the memory measurements run in separate
processes. django-cotton patches Django's template lexer when it loads,
which would change the compile numbers for every other workload, and
peak memory is only meaningful in a fresh process.

## How the numbers are collected

- **Samples.** Each engine first works out how many renders take at
  least 10 ms (3 ms with `--quick`), then the bench takes 9 samples of
  that many renders (5 with `--quick`). The reported time is the median
  sample divided by the number of renders, so even sub-microsecond
  workloads are timed accurately.
- **Fairness.** Engines alternate sample by sample and the starting
  engine rotates, so a change in machine load affects all of them
  equally. Garbage is collected before every sample.
- **Noise.** The spread between samples (standard deviation over mean)
  is kept for every result. Tables mark a result with a yellow ± when
  its samples varied by 5% or more, and a comparison is reported as
  "same" when the difference is within the two results' combined
  spread, with a minimum of 3%.
- **Correctness.** Each engine's output is compared with stock Django's
  before timing. A different output is reported as "wrong output"
  instead of a time, and an engine that cannot run a workload shows the
  reason, such as "not supported" or "syntax error". CSRF token values,
  which change on every render, are ignored in the comparison.
- **Summary.** The summary gives the geometric mean of oxide's speedup
  over the workloads both engines can run, and counts the workloads
  each engine could not run.

## Interpreter builds

Results differ between the regular CPython build and the free-threaded
build (3.14t), and the threads section only shows parallel speedups on
the free-threaded build. To measure the regular build alongside the
free-threaded development environment, use a second environment:

```sh
UV_PROJECT_ENVIRONMENT=.venv-gil uv sync --python 3.14+gil --group dev
VIRTUAL_ENV=.venv-gil uvx maturin develop --release
.venv-gil/bin/python benches/bench.py
```

## Files

| File | Contents |
|------|----------|
| `bench.py` | Command line, sections, and the separate-process workers |
| `cases.py` | Templates, test data, and Django objects |
| `harness.py` | Timing, output checks, and thread throughput |
| `report.py` | Tables, the summary, JSON output, and `compare` |
| `setup_env.py` | Django settings and the three engines |
| `bench_tags.py`, `bench_urls.py` | The custom tag library and URLconf the templates use |
| `templates/` | Templates loaded by name, including the django-cotton components |

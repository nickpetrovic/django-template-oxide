# Contributing

## Build

You need Rust 1.88+ and Python 3.14+.

```sh
uv sync --group dev
uvx maturin develop --release
```

That builds the Rust extension into your local venv. Re-run
`maturin develop` whenever you touch Rust code.

## Test

```sh
uvx maturin develop --release   # rebuild after any Rust change
uv run pytest tests/            # Python suites
cargo test                      # Rust unit tests
scripts/check.sh                # everything above, plus fmt, clippy, rustdoc, unused deps, advisories, and the differential fuzzer
```

The Python suites have 3081 tests:

- 1551 in oxide's own suites: `test_regressions.py`,
  `test_compliance.py`, `test_basic_rendering.py`,
  `test_oxide_backend.py`, `test_parity.py`, and
  `test_django_parity_matrix.py`. The parity matrix renders every
  built-in filter, lookup, tag, and localization case through stock
  Django and oxide and requires the same output, exception type, and
  error message.
- 1530 vendored Django 6.1 `template_tests`, routed through the oxide
  backend. They need Django 6.1; on Django 6.0, run the suite with
  `--ignore=tests/django_template_tests`.

3079 pass and 2 are skipped: a case-insensitive file system guard in
`test_loaders`, and an i18n compilation test that depends on the
environment and fails the same way without oxide on Python 3.14.
There are 302 Rust unit tests.

## Bench

```sh
uv sync --group dev
uv run --no-sync python benches/bench.py
```

`django-rusty-templates` is a `dev` dependency (pulled from git) for
the head-to-head comparison. See `benches/README.md` for what the
workloads measure.

## Docs

The documentation site is built with [Zensical](https://zensical.org)
from the Markdown files in `docs/`, using the settings in `mkdocs.yml`.

```sh
uv run --only-group docs zensical serve   # preview at http://localhost:8000
uv run --only-group docs zensical build   # write the site to site/
```

`--only-group` installs just the docs tools, so building the docs does
not rebuild the Rust extension.

## Project layout

```
src/                      Rust crate (the engine)
  lib.rs                  PyO3 module entry point
  template.rs             Template compile + render orchestrator
  parser.rs               Template parser
  lexer.rs                Tokenizer (pure-Rust path, used standalone)
  nodes.rs                Node trait + TextNode / VariableNode + NodeList
  context.rs              Context + BaseContext + Value
  variable.rs             Variable lookup + FilterExpression
  errors.rs               TemplateError + PyErr round-trip
  utils.rs                SafeString and HTML escaping
  filters/                Built-in filters
    mod.rs                Rust fast paths for Django's defaultfilters; other inputs go to Django's own functions
    date_format.rs        {{ value|date }} formatting and translated month and day names
  tags/                   Built-in tags
    mod.rs                Tag registry, shared helpers, and the smaller tags ({% if %}, {% with %}, {% cycle %}, ...)
    for_tag.rs            {% for %}
    url_tag.rs            {% url %}
    cache_tag.rs          {% cache %}
    loader_tags.rs        {% extends %}, {% block %}, {% include %}
    i18n_tags.rs          {% trans %}, {% blocktranslate %}, etc.
  django_drop_in.rs       PyParser / PyToken / PyNodeList / PyOpaqueNode
  django_integration.rs   NodeList.render acceleration (monkey-patch)
  python_cache.rs         Cached Django module / attribute references
  body_program.rs         JIT bytecode for hot for-loop bodies
  smartif.rs              {% if %} expression parser
  py_bindings.rs          PyTemplate (the Python-facing Template class)
  prof.rs                 Optional per-zone profiler (feature-gated)

python/django_template_oxide/
  __init__.py             Public Python API
  backend.py              OxideTemplates backend + OxideTemplateAdapter
  apps.py                 AppConfig that installs the acceleration patch
  _patch.py               NodeList.render monkey-patch
  _rust.so                Built Rust extension

tests/
  test_regressions.py        Bug-driven regression suite
  test_compliance.py         Django behavioral compliance
  test_basic_rendering.py    Smoke tests
  test_oxide_backend.py      Tests via the OxideTemplates backend path
  test_parity.py             Stock-Django parity for rendering entrypoints,
                             context, filters, CSRF, lexer detection, etc.
  test_django_parity_matrix.py  Every built-in filter, lookup, tag, and
                             localization case through stock Django and oxide
  i18n/                      Vendored Django i18n tests
  django_template_tests/     Vendored Django 6.1 template_tests (1530 tests)

benches/
  bench.py                Comparison bench (oxide vs rusty vs stock)
  perf_drill.py           Micro-profiler for hot-spot work

docs/                     Documentation site source (built with Zensical)
scripts/                  Tooling (Django test sync, etc.)
```

## Workflow

1. Cut a branch.
2. Make changes. If you touch Rust, `maturin develop --release` to
   rebuild.
3. Run tests. If you add behavior, add a test.
4. Run the bench if you touched a hot path. Don't ship perf
   regressions silently; if you accept one for correctness, note it
   in the commit message.
5. Update CHANGELOG.md under `Unreleased`.

## Releasing

Nothing publishes automatically. Pushing commits or tags does not run
any workflow; a release only happens when you start the Release
workflow by hand.

1. Set the new version in `Cargo.toml`.
2. In `CHANGELOG.md`, rename `## Unreleased` to `## X.Y.Z (YYYY-MM-DD)`
   and add an empty `## Unreleased` above it.
3. Commit, then tag the commit and push the tag:

   ```sh
   git tag -a vX.Y.Z -m "X.Y.Z"
   git push origin vX.Y.Z
   ```

4. Start the release on that tag, either from the Actions tab (Release,
   Run workflow, pick the tag) or with:

   ```sh
   gh workflow run release.yml --ref vX.Y.Z
   ```

The workflow checks that the tag matches `Cargo.toml`, runs the tests on
CPython 3.14 and 3.14t against Django 6.0 and 6.1, builds the wheels and
the source distribution, publishes them to PyPI, and creates a GitHub
Release with the matching `CHANGELOG.md` section as its notes.

## Code style

- Rust: `cargo fmt` defaults. `cargo clippy -- -D warnings` clean.
- Python: ruff defaults (line length 88).
- No code comments or docstrings. Explain non-obvious decisions in
  the commit message, and delete existing comments in code you change.
- No em-dashes in any new prose.
- No emojis unless something is being used as data (e.g. a status
  glyph in CLI output).

## Adding a tag

Small tags live in `src/tags/mod.rs`. A larger tag gets its own file
under `src/tags/`, like `for_tag.rs` or `url_tag.rs`:

1. Create `src/tags/your_tag.rs`.
2. Define your `Node` struct with `token_field: Option<Token>` and
   `origin_field: Option<Origin>` fields.
3. `impl Node for YourNode { impl_node_metadata!(); fn render(...) { ... } }`.
4. Define `pub fn compile_your_tag(parser, token) -> Result<Box<dyn Node>>`.
5. In `src/tags/mod.rs`: `pub mod your_tag;`, `pub(crate) use your_tag::{...};`,
   and add `("your_tag", compile_your_tag)` to `register_default_tags`.
6. Add cases to `TAG_CASES` in `tests/test_django_parity_matrix.py`,
   which renders each template through stock Django and oxide and
   requires the same output, exception type, and (in
   `test_error_messages_match_django`) the same error message.

## Reporting bugs

Include:

- Django version, Python version, OS.
- Minimal template + context that reproduces.
- Expected output (what stock Django produces) and actual output.

We hold oxide to byte-for-byte parity with stock Django for any
template the Django docs guarantee. Divergence is a bug.

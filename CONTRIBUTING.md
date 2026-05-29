# Contributing

## Build

You need Rust 1.85+ and Python 3.10+.

```sh
uv sync --group dev
uvx maturin develop --release
```

That builds the Rust extension into your local venv. Re-run
`maturin develop` whenever you touch Rust code.

## Test

```sh
uv run --no-sync python -m pytest tests/test_regressions.py tests/test_basic_rendering.py tests/test_compliance.py tests/test_oxide_backend.py
```

742 Python tests, runs in under a second. Anything else under `tests/` is
vendored Django infrastructure that won't run standalone.

The Rust unit tests (331 tests) are run separately:

```sh
cargo test
```

## Bench

```sh
uv sync --group dev --group bench
uv run --no-sync python benches/bench.py
```

`django-rusty-templates` is pulled from git for head-to-head
comparison. See `benches/README.md` for what the workloads measure.

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
  filters/                Built-in filters (all 57 from defaultfilters)
  tags/                   Built-in tags
    mod.rs                Tag registry + shared helpers
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
  test_regressions.py        Bug-driven regression suite (530+ tests)
  test_compliance.py         Django 6.0 behavioral compliance (200+ tests)
  test_basic_rendering.py    Smoke tests
  test_oxide_backend.py      Tests via the OxideTemplates backend path
  django_template_tests/     Vendored Django template-tests fixtures

benches/
  bench.py                Comparison bench (oxide vs rusty vs stock)
  perf_drill.py           Micro-profiler for hot-spot work

docs/                     mkdocs site source
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

## Code style

- Rust: `cargo fmt` defaults. `cargo clippy -- -D warnings` clean.
- Python: ruff defaults (line length 88).
- Comments: explain why, not what. Cite Django source line numbers
  when porting behavior so future readers can verify.
- No em-dashes in any new prose.
- No emojis unless something is being used as data (e.g. a status
  glyph in CLI output).

## Adding a tag

The pattern after the section 5 refactor is one tag per file:

1. Create `src/tags/your_tag.rs`.
2. Define your `Node` struct with `token_field: Option<Token>` and
   `origin_field: Option<Origin>` fields.
3. `impl Node for YourNode { impl_node_metadata!(); fn render(...) { ... } }`.
4. Define `pub fn compile_your_tag(parser, token) -> Result<Box<dyn Node>>`.
5. In `src/tags/mod.rs`: `pub mod your_tag;`, `pub(crate) use your_tag::{...};`,
   and add `("your_tag", compile_your_tag)` to `register_default_tags`.
6. Add a test in `tests/test_regressions.py` or `tests/test_compliance.py`
   that renders the same template through stock Django and oxide and
   asserts byte-equal output.

## Reporting bugs

Include:

- Django version, Python version, OS.
- Minimal template + context that reproduces.
- Expected output (what stock Django produces) and actual output.

We hold oxide to byte-for-byte parity with stock Django for any
template the Django docs guarantee. Divergence is a bug.

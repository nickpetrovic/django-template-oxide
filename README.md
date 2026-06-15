# django-template-oxide

A Rust implementation of Django's template engine, drop-in compatible
with `django.template`.

## Disclaimer

This is probably AI slop. I sat down with Claude and pair-built a
100% Django-compliant template engine in Rust. The goal was twofold:

1. Stand up a full implementation, not a partial one.
2. Beat `django-rusty-templates` on benchmarks without waiting for them
   to reach full compliance with Django.

Both goals are met as of now. If you read the code and notice
inhuman regularity, suspicious thoroughness in the comments, or a
docstring that cites a specific Django source line you didn't ask
for, that's why. It's been reviewed and exercised end-to-end against
a real Django app, but the volume of code is what it is.

## What it does

You add it as a `TEMPLATES` backend. Your templates render through
Rust instead of Python. Everything you can do in Django templates,
you can do here, including:

- All built-in tags and filters (Django 6.0 compliance verified
  against 1293 tests: 962 Python regression/compliance tests and
  331 Rust unit tests).
- Template inheritance: `{% extends %}` / `{% block %}` / `{% include %}`.
- Custom Python tags / filters loaded via `{% load %}`.
- i18n, static, cache, l10n, tz tag libraries.
- Third-party libraries that monkey-patch `Lexer.tokenize`, including
  django-cotton.

## Install

```sh
pip install django-template-oxide
```

(Not on PyPI yet. For now, install editable from the repo:
`pip install -e .` after cloning.)

## Use

Replace the default Django backend in your `settings.py`:

```python
TEMPLATES = [
    {
        "BACKEND": "django_template_oxide.backend.OxideTemplates",
        "DIRS": [...],
        "APP_DIRS": True,
        "OPTIONS": {
            "context_processors": [...],
            # any other options you'd pass to django.template.backends.django.DjangoTemplates
        },
    },
]
```

That's it. Render via `django.shortcuts.render`,
`django.template.loader.get_template`, the
`{% include %}`/`{% extends %}` chains, custom tag libraries, all
unchanged.

## Performance

Numbers below are oxide against `django-rusty-templates` and stock
Django, items=50, iters=200, on an M-series laptop. Smaller numbers
are better. The full bench (30 render cases, 3 compile sizes,
scaling sweep) lives in `benches/bench.py`.

| Workload                | Oxide    | Rusty       | Stock     |
|-------------------------|----------|-------------|-----------|
| TEXT ONLY               | 0.005ms  | 0.011ms     | 0.019ms   |
| VARS ONLY (3 attrs)     | 0.019ms  | 0.159ms     | 0.296ms   |
| FULL TEMPLATE           | 0.104ms  | 0.836ms     | 1.513ms   |
| FILTER CHAIN (6-deep)   | 0.032ms  | unsupported | 0.684ms   |
| WITH NESTED (4 levels)  | 0.103ms  | unsupported | 0.754ms   |
| NESTED LOOP (apps×tags) | 0.061ms  | 0.123ms     | 0.784ms   |
| URL TAG                 | 0.455ms  | 0.543ms     | 0.722ms   |
| INCLUDE LOOP            | 0.042ms  | unsupported | 0.457ms   |
| INHERITANCE             | 0.037ms  | unsupported | 0.344ms   |
| Compile SMALL (10 rows) | 0.158ms  | 0.189ms     | 0.915ms   |
| Compile MEDIUM (100)    | 1.38ms   | 14.29ms     | 9.52ms    |
| Compile LARGE (500)     | 6.93ms   | 349.05ms    | 49.38ms   |

Oxide wins every render and compile workload, including small
template compilation where it now beats rusty (0.158ms vs 0.189ms).
Compile time scales linearly while rusty grows superlinearly. See
`benches/README.md` for methodology and how to reproduce.

## Compatibility

| Component | Versions tested      |
|-----------|----------------------|
| Django    | 4.2 LTS, 5.0, 5.1, 5.2, 6.0 |
| Python    | 3.10, 3.11, 3.12, 3.13, 3.14 |
| Platform  | macOS (Apple Silicon and Intel), Linux x86_64 and aarch64 |

Windows is not actively tested but the Rust + PyO3 stack supports
it; if you run it on Windows and something breaks, file an issue.

## What we don't do

- No async template rendering. Django itself doesn't have it
  (verified against Django main as of writing). When Django adds
  `Template.arender`, we'll match.
- No Jinja2 backend. We replace `django.template`, not Jinja.
- The Rust-side AST is not stable API. If you reach into `_rust`
  internals from Python, expect breakage. The supported API is the
  `TEMPLATES` backend, the `{% load %}`-style custom tag interface,
  and `django.template.loader.get_template(...)`.

## Acknowledging django-rusty-templates

[django-rusty-templates](https://github.com/LilyFirefly/django-rusty-templates)
is an earlier Rust-backed Django template engine (repo created
September 2024). Their own README describes it as experimental and
"not yet ready for full release". At the time of this writing it
returns `NotImplementedError` for several common tags (`{% with %}`,
`{% cycle %}`, `{% spaceless %}`, custom `@register.tag`) and for
`{% include %}` / `{% extends %}` template loading.

Oxide went the other direction: ship 100% Django 6.0 compliance now,
including third-party hooks like cotton's `Lexer.tokenize` patch,
even if that means calling back into Python in places where rusty
goes pure-Rust. We verify against Django's own
`tests/template_tests/` suite (1513 of 1514 pass, 1 skipped on
case-insensitive filesystems, 0 failures).

When rusty reaches full compliance the comparison will be more
meaningful. Today, the bench in this repo runs both and reports
where rusty bails. Make your own call.

## Testing

Build the extension, then run the suites:

```sh
uv sync --group dev
uvx maturin develop --release   # rebuild after any Rust change
uv run pytest tests/            # 2476 Python tests (2474 pass, 2 skipped)
cargo test                      # 331 Rust unit tests
```

The Python suite is 962 tests in oxide's own regression/compliance
suite plus 1514 vendored Django `template_tests` routed through the
oxide backend. See [CONTRIBUTING.md](./CONTRIBUTING.md) for the
breakdown.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for build instructions,
test commands, and the code organization.

## License

MIT. See [LICENSE](./LICENSE).

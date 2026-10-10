# django-template-oxide

A Rust implementation of Django's template engine. Add it as a `TEMPLATES` backend and your templates render through Rust instead of Python, with the same output, the same exceptions, and the same error messages as stock Django.

It passes 1529 of Django 6.1's own 1530 `template_tests` (one is skipped on case-insensitive file systems), plus a parity suite that renders every built-in filter, tag, lookup, and localization case through both engines and requires identical results.

This project was pair-built with Claude. See [the docs](docs/index.md) for what that means for the code.

## Install

```sh
pip install django-template-oxide
```

Requires Python 3.14+ and Django 6.0+. Wheels are published for Linux (x86_64 and arm64) and macOS (Apple Silicon), for both the regular and free-threaded builds of CPython 3.14. See [Installing](docs/install.md) to build from source.

## Use

```python
TEMPLATES = [
    {
        "BACKEND": "django_template_oxide.backend.OxideTemplates",
        "DIRS": [...],
        "APP_DIRS": True,
        "OPTIONS": {...},
    },
]
```

Nothing else changes: `render`, `get_template`, `{% extends %}`, `{% include %}`, and custom tag libraries work as before.

## Performance

Time per render or compile. Smaller is better.

| Workload                          | Oxide    | django-rusty-templates | Stock Django |
|-----------------------------------|---------:|-----------------------:|-------------:|
| Full table, 50 rows               | 41.4 µs  | 359.6 µs               | 760.1 µs     |
| Date filters, 50 rows             | 52.0 µs  | 450.5 µs               | 712.9 µs     |
| `{% url %}`, 50 rows              | 212.9 µs | 246.3 µs               | 354.4 µs     |
| `{% extends %}` with three blocks | 10.5 µs  | not supported          | 120.9 µs     |
| Django admin index page           | 410.0 µs | not supported          | 529.7 µs     |
| Compile, 500 rows                 | 4.01 ms  | 169.5 ms               | 24.7 ms      |

Across 50 workloads, oxide averages 6.2× faster than stock Django and 3.2× faster than django-rusty-templates on the 32 workloads that engine can run. Measured on a MacBook Pro (Apple M5 Max, 64 GB, macOS 27.0.1) with the regular CPython 3.14 build; the free-threaded build gives the same results to within a few percent. The full results, the software versions, and how to reproduce them are in [Performance](docs/performance.md).

## Documentation

- [Installing](docs/install.md)
- [Using](docs/usage.md)
- [Compatibility](docs/compatibility.md)
- [Custom tags and filters](docs/extending.md)
- [Performance](docs/performance.md)
- [Limitations](docs/limitations.md)
- [Contributing](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)

## License

MIT. See [LICENSE](./LICENSE).

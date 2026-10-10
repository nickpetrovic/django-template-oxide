# Installing

## From PyPI

```sh
pip install django-template-oxide
```

Wheels are published for Linux (x86_64 and arm64) and macOS (Apple
Silicon), for both the regular and free-threaded (3.14t) builds of
CPython 3.14. On other platforms pip builds from the source
distribution, which needs a Rust toolchain.

## From source

```sh
git clone https://github.com/nickpetrovic/django-template-oxide.git
cd django-template-oxide
uv sync --group dev
uvx maturin develop --release
```

That gives you an editable install. Re-run `uvx maturin develop --release`
after any change to the Rust source.

## Requirements

- Python 3.14 or newer
- Django 6.0 or newer
- Rust 1.88+ (only when building from source)

## Verifying

```python
>>> import django_template_oxide
>>> django_template_oxide.__version__
'0.1.0'
>>> from django_template_oxide.backend import OxideTemplates
>>> OxideTemplates
<class 'django_template_oxide.backend.OxideTemplates'>
```

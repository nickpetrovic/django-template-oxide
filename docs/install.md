# Installing

## From PyPI

Once the first release ships:

```sh
pip install django-template-oxide
```

## From source (current state)

```sh
git clone https://github.com/nickpetrovic/django-template-oxide.git
cd django-template-oxide
uv sync --group dev
uvx maturin develop --release
```

That gives you an editable install. Re-run `uvx maturin develop --release`
after any change to the Rust source.

## Requirements

- Python 3.10 or newer
- Django 4.2 or newer
- Rust 1.85+ (only when building from source)

## Verifying

```python
>>> import django_template_oxide
>>> django_template_oxide.__version__
'0.1.0'
>>> from django_template_oxide.backend import OxideTemplates
>>> OxideTemplates
<class 'django_template_oxide.backend.OxideTemplates'>
```

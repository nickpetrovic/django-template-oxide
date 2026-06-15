# django-template-oxide

A Rust implementation of Django's template engine, drop-in compatible
with `django.template`.

## What it is

You add it as a `TEMPLATES` backend. Your templates render through
Rust instead of Python. Everything you do in Django templates works
unchanged.

- All Django built-in tags and filters.
- Template inheritance: `{% extends %}` / `{% block %}` / `{% include %}`.
- Custom Python tags and filters loaded via `{% load %}`.
- i18n, static, cache, l10n, and tz tag libraries.
- Third-party libraries that monkey-patch `Lexer.tokenize`,
  including django-cotton.

Behavioral compliance verified against Django's own
`tests/template_tests/` suite: 1513 of 1514 tests pass on Django 6.0
(1 skipped on case-insensitive filesystems, 0 failures).

## What it's not

- Not a Jinja2 backend. This replaces `django.template`, not Jinja.
- Not async. Django itself has no `Template.arender` (as of Django
  6.0 / main); when Django adds one, we'll match.
- Not a fork of Django. We reuse Django's loader chain, settings,
  and Python tag-library API end-to-end.

## How fast

| Workload                | Oxide    | django-rusty-templates | Stock Django |
|-------------------------|----------|------------------------|--------------|
| TEXT ONLY               | 0.005ms  | 0.011ms                | 0.019ms      |
| VARS ONLY (3 attrs)     | 0.019ms  | 0.159ms                | 0.296ms      |
| FULL TEMPLATE           | 0.104ms  | 0.836ms                | 1.513ms      |
| INHERITANCE             | 0.071ms  | unsupported            | 0.344ms      |
| Compile LARGE (500 rows)| 6.93ms   | 349.05ms               | 49.38ms      |

See [Performance](performance.md) for the full benchmark and how to
reproduce it.

## Getting started

Read [Installing](install.md) and then [Using](usage.md). For
specifics on what works in which Django/Python combination, see
[Compatibility](compatibility.md).

## Honest disclaimer

This project was pair-built with Claude. The volume of code and the
density of the documentation is what it is. Code has been reviewed
and exercised end-to-end against real Django apps and Django's own
test suite, but if the regularity feels uncanny, that's why.

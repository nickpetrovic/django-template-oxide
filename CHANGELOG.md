# Changelog

All notable changes are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
follow [SemVer](https://semver.org/spec/v2.0.0.html).

Until the first tagged release, everything lives under `Unreleased`.

## Unreleased

### Added

- Drop-in `OxideTemplates` backend for `TEMPLATES`. Configures the
  Rust render path with no other code changes.
- Full Django 6.0 tag library compliance: `{% if %}`, `{% for %}`,
  `{% extends %}` / `{% block %}` / `{% include %}`, `{% with %}`,
  `{% cycle %}`, `{% url %}`, `{% csrf_token %}`, `{% spaceless %}`,
  `{% autoescape %}`, `{% verbatim %}`, `{% load %}`, `{% now %}`,
  `{% filter %}`, `{% firstof %}`, `{% widthratio %}`,
  `{% ifchanged %}`, `{% cache %}`, `{% regroup %}`, `{% comment %}`,
  `{% templatetag %}`, `{% partialdef %}` / `{% partial %}`,
  `{% resetcycle %}`.
- Full Django 6.0 defaultfilter compliance: all 57 filters from
  `django.template.defaultfilters` plus `django.contrib.humanize`.
- i18n tags: `{% trans %}`, `{% blocktranslate %}`, `{% language %}`,
  language-info accessors.
- Static / cache / l10n / tz tag libraries.
- Custom `@register.tag`, `@register.simple_tag`,
  `@register.simple_block_tag`, `@register.inclusion_tag`, and
  `@register.filter` all work through the standard `Library` API.
- Cotton-style `Lexer.tokenize` monkey-patches honored.
- Regression suite of 742 tests covering compliance, edge cases,
  third-party compat patterns, and the public `OxideTemplates`
  backend path.
- Benchmark suite (`benches/bench.py`) comparing oxide against stock
  Django and `django-rusty-templates` across 22 render workloads, 3
  compile sizes, and a scaling sweep.

### Performance

Numbers measured against `django-rusty-templates` and stock Django
on an M-series laptop. See `benches/README.md` for methodology.

- Beats `django-rusty-templates` on every workload it can run.
- Compile time scales linearly while rusty grows superlinearly:
  oxide 12ms vs rusty 380ms on a 500-row template.
- Render path is 5x-20x faster than stock Django across the board.

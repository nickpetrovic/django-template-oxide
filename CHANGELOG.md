# Changelog

All notable changes are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
follow [SemVer](https://semver.org/spec/v2.0.0.html).

Until 1.0, the Python API surface and behavior may change between
minor versions.

## Unreleased

## 0.1.0 (2026-10-09)

First release.

### Added

- Drop-in `OxideTemplates` backend for `TEMPLATES`. Configures the
  Rust render path with no other code changes.
- Full Django 6.0 and 6.1 tag library compliance: `{% if %}`, `{% for %}`,
  `{% extends %}` / `{% block %}` / `{% include %}`, `{% with %}`,
  `{% cycle %}`, `{% url %}`, `{% csrf_token %}`, `{% spaceless %}`,
  `{% autoescape %}`, `{% verbatim %}`, `{% load %}`, `{% now %}`,
  `{% filter %}`, `{% firstof %}`, `{% widthratio %}`,
  `{% ifchanged %}`, `{% cache %}`, `{% regroup %}`, `{% comment %}`,
  `{% templatetag %}`, `{% partialdef %}` / `{% partial %}`,
  `{% resetcycle %}`, and Django 6.1's `{% csp_nonce_attr %}`.
- Full Django 6.0 and 6.1 defaultfilter compliance: all 57 filters from
  `django.template.defaultfilters` plus `django.contrib.humanize`.
- i18n tags: `{% trans %}`, `{% blocktranslate %}`, `{% language %}`,
  language-info accessors.
- Static / cache / l10n / tz tag libraries.
- Custom `@register.tag`, `@register.simple_tag`,
  `@register.simple_block_tag`, `@register.inclusion_tag`, and
  `@register.filter` all work through the standard `Library` API.
- Cotton-style `Lexer.tokenize` monkey-patches honored.
- 1551 tests of oxide's own covering compliance, edge cases,
  third-party compat patterns, and the public `OxideTemplates`
  backend path, plus 1530 vendored Django 6.1 `template_tests` routed
  through the oxide backend (1529 pass, 1 skipped, 0 failures).
- Parity suite (`tests/test_django_parity_matrix.py`, 483 tests) that
  renders every built-in filter against every kind of value, every
  lookup style, the tags, and localized output through stock Django
  and oxide, and requires the same output, exception type, and error
  message.
- Benchmark suite (`benches/bench.py`) comparing oxide against stock
  Django and `django-rusty-templates` across 30 render workloads, 3
  compile sizes, a scaling sweep, and a context-entry section.

### Changed

- Requires Python 3.14+ and Django 6.0+. Earlier versions are no
  longer supported.
- A filter argument that names a missing variable, or looks up an
  attribute through `None` or past the end of a list, now raises
  `VariableDoesNotExist`, as Django does. Earlier versions rendered an
  empty string. Templates that relied on this, such as
  `{{ user.full_name|default:user.email }}` when `user` can be `None`,
  need `{% firstof %}` or an `{% if %}` guard.
- A filter called with the wrong number of arguments now raises
  `TemplateSyntaxError` when the template is compiled, using Django's
  own argument check.
- A `{% load %}` library that replaces a built-in filter now runs its
  own version; the Rust fast path only runs Django's built-in filter
  functions.
- The release build is no longer stripped. Stripped builds failed to
  load on recent macOS; the extension is about 190 KB larger.

### Fixed

- Attribute lookups inside a nested `{% for %}` no longer return the
  outer loop's values.
- Lookups of `str` methods such as `{{ value.strip }}` no longer
  render empty, and an empty string is no longer treated as a missing
  variable.
- Every built-in filter now matches Django's output for every kind of
  value; inputs the Rust fast path does not handle are passed to
  Django's own filter function.
- Rendering follows Django's `render_value_in_context`: aware
  datetimes are converted to local time, and numbers, dates, and
  times are localized, including the language's decimal separator.
- The `date` filter follows Django's format parsing, uses translated
  month and day names, and picks up a language switched in the middle
  of a render.
- `{% if %}` follows Django's operator rules, including errors inside
  comparisons, `and` / `or` returning their operands, and `is`.
- `{% for %}` calls `len()` once like Django, lists `forloop` keys in
  Django's order, makes `forloop` visible to included templates and
  custom tags, and gives unpacked loop variables their own context
  layer.
- `{% firstof %}`, `{% cycle %}`, `{% now %}`, `{% widthratio %}`,
  `{% ifchanged %}`, `{% filter %}`, `{% url %}`, `{% cache %}`,
  `{% include %}`, `{% extends %}`, and `{% language %}` match Django,
  including their error messages. `{% url %}` reads `current_app`
  from the request.
- Integer literals with underscores (`1_000`) or beyond 64 bits are
  parsed like Python.

### Performance

Numbers measured against `django-rusty-templates` and stock Django;
the machine, versions, and full results are in `docs/performance.md`.

- Beats `django-rusty-templates` on every render workload it can run,
  and ties it on compiling a small template.
- Compile time scales linearly while rusty grows superlinearly:
  oxide 3.7ms vs rusty 167ms on a 500-row template.
- Render path is 5x-40x faster than stock Django on typical workloads.
- Floats render in Rust with the language's decimal separator instead
  of going through Python.
- `{% extends %}` caches compiled parent templates across renders
  instead of re-parsing them each time, roughly halving inheritance
  render time.

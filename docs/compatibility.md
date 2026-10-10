# Compatibility

## Django versions

Django 6.0 and newer. Earlier versions are not supported. Every
release is tested against Django 6.1 with the full test suite, and
against Django 6.0 with oxide's own suites (Django's vendored
`template_tests` are from 6.1 and use features 6.0 does not have).

The behavioral compliance bar is byte-equal output with stock Django
for any template the Django documentation guarantees. We run two
test suites that hold us to this:

1. **Oxide's own suites** (1551 tests): regression and compliance
   tests compare `OxideTemplate(src).render(ctx)` against
   `django.template.Engine.from_string(src).render(Context(ctx))`
   byte-for-byte across every documented tag, filter, and edge case.
   The parity suite (`tests/test_django_parity_matrix.py`, 483 tests)
   renders every built-in filter against every kind of value, every
   lookup style, the tags, and localized output through both stock
   Django and oxide, and requires the same output, exception type,
   and error message.

2. **Django's own `tests/template_tests/`** (1530 tests, from Django
   6.1): vendored into `tests/django_template_tests/` and run through
   the oxide backend. 1529 pass, 1 skipped, 0 failures.

No Django template test fails. The single skip is a
case-insensitive-filesystem guard in `test_loaders`, not a
compliance gap.

## Python versions

Python 3.14 and newer, including the free-threaded build (3.14t).
Earlier versions are not supported.

## Platforms

| Platform              | Status         |
|-----------------------|----------------|
| macOS aarch64         | Tested daily   |
| macOS x86_64          | Should work    |
| Linux x86_64          | Should work    |
| Linux aarch64         | Should work    |
| Windows               | Untested       |

## Third-party libraries that hook the template system

| Library                  | Compatibility |
|--------------------------|---------------|
| django-cotton            | Verified: source preprocessing via `Lexer.tokenize` works |
| django-debug-toolbar     | Untested but expected to work (uses standard tag API) |
| django-template-partials | Verified: `parser.extra_data['partials']` round-trips |
| jinja2                   | Out of scope, oxide replaces `django.template`, not Jinja |

If you use a third-party tag library that hooks template internals,
test it. File an issue if it breaks. The compatibility surface is
documented in `src/django_drop_in.rs`.

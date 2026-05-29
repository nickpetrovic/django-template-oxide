# Compatibility

## Django versions

Tested against 4.2 LTS, 5.0, 5.1, 5.2, and 6.0.

The behavioral compliance bar is byte-equal output with stock Django
for any template the Django documentation guarantees. We run two
test suites that hold us to this:

1. **Oxide's own regression suite** (962 tests): compares
   `OxideTemplate(src).render(ctx)` against
   `django.template.Engine.from_string(src).render(Context(ctx))`
   byte-for-byte across every documented tag, filter, and edge case.

2. **Django's own `tests/template_tests/`** (1514 tests): we clone
   Django at a tag, swap the `TEMPLATES` backend for oxide, run
   Django's own template test suite. 1513 pass, 1 skipped, 0 failures.

No Django template test fails. The single skip is a
case-insensitive-filesystem guard in `test_loaders`, not a
compliance gap.

## Python versions

Tested on 3.10, 3.11, 3.12, 3.13, 3.14. We use 3.14 as the primary
development target.

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

# Limitations

## No async render

Django itself does not have async template rendering as of Django
6.0 / main. There is no `Template.arender`, no `aget_template`, no
async render path in `django.template.base`. We match Django.

If you need to render templates from an async view, do what Django
docs recommend: wrap the sync render in `asgiref.sync.sync_to_async`.

## Rust-side AST is not public API

Anything you can reach via `django_template_oxide._rust.*` may
change between versions. The supported API is:

- `django_template_oxide.backend.OxideTemplates` as a `TEMPLATES`
  backend.
- `django.template.loader.get_template` and friends, routed through
  the backend.
- The standard `{% load %}` / `@register.tag` / `@register.filter`
  custom-tag interface.

Tags that need access to internal parser state should use the same
`parser.compile_filter`, `token.split_contents`, `parser.parse`
methods that stock Django tags use. We mirror that API.

## Compiling small templates is no faster than rusty

On a template of about 120 nodes, oxide and django-rusty-templates
compile in the same time (0.087 ms and 0.088 ms). Oxide tokenizes
through Django's Python `Lexer.tokenize` so that third-party
libraries which patch it, such as django-cotton, keep working, and
on a template that small that cost is a large share of the total.

The trade is intentional: a Rust-native lexer broke projects that
use Cotton. Cotton patches the lexer when its app loads, before the
first compile, so oxide cannot tell a stock lexer from a patched one.

On larger templates the cost is spread out: compiling 500 rows takes
3.74 ms in oxide against 166.98 ms in rusty and 23.04 ms in stock
Django.

## Templates compiled once, cached forever

`OxideTemplates.get_template(name)` caches the compiled adapter
indefinitely. The Rust-side `TEMPLATE_CACHE` thread-local also
caches the parsed nodelist for templates loaded via
`{% include %}` / `{% extends %}`.

There is currently no cache invalidation hook. In production this
is fine (templates don't change). In dev with autoreload it means
template edits don't take effect until process restart. A
`clear_template_caches()` function will land before 1.0.

## Free-threaded Python (3.14t)

Importing the extension on a free-threaded build leaves the GIL
disabled, and the development test suite and benchmarks run on
CPython 3.14t. Per-render caches are thread-local and shared caches
are behind locks.

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

## Performance gap on COMPILE SMALL

Oxide pays a small overhead per compile vs stock Django on very
small templates (sub-200 nodes). The overhead comes from going
through Django's Python `Lexer.tokenize` to honor third-party
monkey patches (django-cotton's source preprocessing pattern).

The trade is intentional: shipping a Rust-native lexer fast path
broke Cotton-using projects in production. Cotton patches at
`AppConfig.ready` time, before our first compile; we can't
distinguish "stock Django" from "cotton-patched" by identity.

For large templates the overhead is amortized and oxide still wins
comfortably: at 1000 items the FULL TEMPLATE renders ~8x faster than
rusty and ~15x faster than stock.

## Templates compiled once, cached forever

`OxideTemplates.get_template(name)` caches the compiled adapter
indefinitely. The Rust-side `TEMPLATE_CACHE` thread-local also
caches the parsed nodelist for templates loaded via
`{% include %}` / `{% extends %}`.

There is currently no cache invalidation hook. In production this
is fine (templates don't change). In dev with autoreload it means
template edits don't take effect until process restart. A
`clear_template_caches()` function will land before 1.0.

## Free-threaded Python (3.13t / 3.14t)

The extension targets the standard GIL build and is not yet validated
against free-threaded (no-GIL) Python. The engine is GIL-bound today (it
holds the GIL across a render via `Python::attach`) and uses thread-local
caches, so it is thread-safe under the GIL but does not advertise
`gil_used = false`. Free-threaded support is a post-1.0 consideration.

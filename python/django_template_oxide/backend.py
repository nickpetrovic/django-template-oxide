"""Django template backend powered by the Rust oxide engine.

Drop-in replacement for ``django.template.backends.django.DjangoTemplates``.

Usage in ``settings.py``::

    TEMPLATES = [{
        "BACKEND": "django_template_oxide.backend.OxideTemplates",
        "DIRS": [...],
        "APP_DIRS": True,
        "OPTIONS": {...},
    }]

Differences from :class:`django.template.backends.django.DjangoTemplates`:

  - ``from_string()`` returns an :class:`OxideTemplateAdapter` wrapping
    our Rust :class:`Template`; the Rust engine compiles the source.
  - ``get_template()`` uses Django's standard loader chain to obtain
    template source, then compiles it with the Rust engine.
  - Every Django built-in tag and filter is implemented in Rust; user
    tags / filters dispatch through Python callbacks.
  - ``{% extends %}`` and ``{% include %}`` use Django's loader chain
    to resolve names (so app templates, cached loaders, and third-party
    loaders like Cotton's all continue to work).

Identical: template syntax (Django syntax, same parser semantics,
auto-escape rules, context-processor pipeline), the
``render(context, request)`` contract, and
``django.template.loader.get_template`` / ``select_template`` /
``render_to_string``.
"""

from django.template import TemplateDoesNotExist
from django.template.backends.django import DjangoTemplates, copy_exception, reraise
from django.template.context import make_context
from django.utils.safestring import mark_safe

try:
    from django.test.signals import template_rendered
except ImportError:
    template_rendered = None

from django_template_oxide._rust import Template as _RustTemplate, Context as _RustContext


# Third-party compatibility shims.
#
# Django's `NodeList.render` in DEBUG mode catches every exception from
# a child node's `render_annotated` and tries to enrich it by calling
# `context.render_context.template.get_exception_info(exc, token)`. The
# `context.render_context.template` reference is whatever the
# currently-active template put there via `push_state(self)`. Stock
# Django's `Template` implements `get_exception_info`; third-party
# template-like objects (notably django-cotton's `InlineTemplate`) do
# not. Without this shim, any exception raised during a cotton inline
# template render surfaces as `'InlineTemplate' object has no attribute
# 'get_exception_info'`, masking the actual error.
#
# Fix: at backend-import time, duck-type the missing method as a no-op
# returning an empty `template_debug` dict. The real error then
# propagates through with its original message and traceback intact.
def _patch_third_party_template_classes():
    try:
        from django_cotton.templatetags import InlineTemplate
        if not hasattr(InlineTemplate, "get_exception_info"):
            def _noop_get_exception_info(self, exception, token):
                return {}
            InlineTemplate.get_exception_info = _noop_get_exception_info
    except ImportError:
        pass


_patch_third_party_template_classes()


class OxideTemplates(DjangoTemplates):
    """Django template backend that compiles + renders via the Rust oxide engine.

    Subclasses :class:`DjangoTemplates` so all of Django's loader/engine
    configuration is honoured. Only ``from_string`` and ``get_template``
    are overridden to swap Django's :class:`django.template.base.Template`
    for our Rust-native :class:`Template`.

    Drop-in compatibility with third-party Django apps
    --------------------------------------------------

    Third-party apps (django-cotton, django-allauth's template scanner,
    debug_toolbar's loader injection, etc.) often wire themselves into a
    specific ``TEMPLATES`` entry at :meth:`AppConfig.ready` by iterating
    ``settings.TEMPLATES`` and matching on ``NAME`` or the trailing
    component of ``BACKEND``. These hooks look for ``"django"``.

    Supported configuration: add ``"NAME": "django"`` to your TEMPLATES
    entry. That hint is enough for third-party AppConfigs to wire into
    our backend transparently. No library-specific code lives here.

    Adapter identity stability
    --------------------------

    :meth:`get_template` caches the returned ``OxideTemplateAdapter`` by
    template name. Required for correctness with third-party tag
    libraries that use ``id(template)`` as a cache key (notably
    ``django-cotton``'s ``CottonComponentNode._vars_node_cache``).
    CPython reuses memory addresses after garbage collection, so
    transient ``Template`` objects can produce ``id()`` collisions.
    Keeping every adapter alive for the process lifetime makes those
    ids stable.

    Tradeoff: template hot-reload in ``DEBUG`` is disabled; restart the
    dev server to pick up template-source changes. Production
    deployments are unaffected (cached loaders already do this).
    """

    def __init__(self, params):
        super().__init__(params)
        # Name-keyed cache of compiled adapters. ``setdefault`` makes
        # inserts race-free under the GIL.
        self._template_cache: dict[str, "OxideTemplateAdapter"] = {}

    def from_string(self, template_code):
        try:
            rust_template = _RustTemplate(template_code, engine=self.engine)
        except Exception as exc:
            raise exc
        return OxideTemplateAdapter(rust_template, self, name=None, origin=None)

    def get_template(self, template_name):
        cached = self._template_cache.get(template_name)
        if cached is not None:
            return cached

        # Use Django's loader chain to fetch the (preprocessed) source +
        # origin. We only use the source to compile our own template.
        try:
            dj_template = self.engine.get_template(template_name)
        except TemplateDoesNotExist as exc:
            reraise(exc, self)

        # `dj_template` is a fully-compiled Django Template. We re-compile
        # from `.source` so the AST runs in Rust; the Django Template
        # acts as canonical source-of-truth for `.origin`.
        source = dj_template.source
        try:
            rust_template = _RustTemplate(
                source,
                engine=self.engine,
                origin=dj_template.origin,
                name=template_name,
            )
        except Exception:
            raise
        adapter = OxideTemplateAdapter(
            rust_template, self, name=template_name, origin=dj_template.origin
        )
        # Race-free insert: if another thread populated the slot first,
        # use theirs so the world sees one canonical instance per name.
        return self._template_cache.setdefault(template_name, adapter)


class OxideTemplateAdapter:
    """Template object returned by ``OxideTemplates.from_string`` and
    ``OxideTemplates.get_template``.

    Same API surface as :class:`django.template.backends.django.Template`:
    ``.origin`` and ``.render(context=None, request=None) -> str``.

    Render path:
      1. Builds a Django Context via :func:`make_context` (handles
         autoescape, RequestContext context processors).
      2. Converts the Django Context to a Rust :class:`Context` by
         flattening the dict stack.
      3. Calls Rust ``Template.render`` which iterates the Rust AST.

    Custom Python nodes/filters encountered during render call back
    into Python via PyOpaqueNode / call_python_filter, passing the Rust
    Context (which exposes the full Django Context API). Mutations
    from Python nodes propagate back through the
    ``render_with_borrowed_context`` mem::swap bridge.
    """

    def __init__(self, rust_template, backend, name=None, origin=None):
        self.template = rust_template
        self.backend = backend
        self._name = name
        self._origin = origin

    @property
    def name(self):
        return self._name

    @property
    def origin(self):
        return self._origin if self._origin is not None else getattr(
            self.template, "origin", None
        )

    def render(self, context=None, request=None):
        # Fast path: a plain dict (or None) with no request skips the
        # entire Django Context wrapping ceremony. About a 3x wall-clock
        # speedup on tiny templates (FOR EMPTY ~4.5us -> ~1.5us).
        #
        # Skipped work:
        #   - `make_context(dict, None, autoescape)` (~1us).
        #   - `push_state(self)` + `bind_template(self)` (~1us). The
        #     Rust side does its own template-binding via
        #     `rust_context.template = ...`. We re-enter the slow path
        #     below when a Django/RequestContext is passed.
        #   - `dj_ctx.flatten()` + `_RustContext(flat, ...)` (~1us).
        #     Rust `Template.render` accepts a plain dict via
        #     py_bindings.rs:957-964.
        #
        # SAFETY: we still need `render_context.push_state(self)` for
        # any custom tag using `context.render_context[self]` (the
        # {% cycle %} pattern). The Rust render path emulates this via
        # its own per-render `RenderContext::push_state`; see
        # context.rs RenderContext.
        if request is None and (context is None or type(context) is dict):
            if template_rendered is not None and template_rendered.receivers:
                from django.template import Context as DjContext
                template_rendered.send(
                    sender=self, template=self,
                    context=DjContext(context or {}),
                )
            try:
                return mark_safe(self.template.render(context))
            except TemplateDoesNotExist as exc:
                reraise(exc, self.backend)

        # Slow path: RequestContext, Django Context, or anything exotic.
        dj_ctx = make_context(
            context, request, autoescape=self.backend.engine.autoescape
        )

        if template_rendered is not None:
            template_rendered.send(sender=self, template=self, context=dj_ctx)

        try:
            with dj_ctx.render_context.push_state(self):
                if dj_ctx.template is None:
                    with dj_ctx.bind_template(self):
                        dj_ctx.template_name = self._name
                        return self._render_through_rust(dj_ctx)
                else:
                    return self._render_through_rust(dj_ctx)
        except TemplateDoesNotExist as exc:
            reraise(exc, self.backend)

    def _render_through_rust(self, dj_ctx):
        flat = dj_ctx.flatten()
        rust_ctx = _RustContext(
            flat,
            autoescape=dj_ctx.autoescape,
            use_l10n=getattr(dj_ctx, "use_l10n", None),
            use_tz=getattr(dj_ctx, "use_tz", None),
            string_if_invalid=self.backend.engine.string_if_invalid or None,
        )
        return mark_safe(self.template.render(rust_ctx))

    @property
    def engine(self):
        # Used by context.bind_template and render_context.push_state
        # to access debug settings. Mirror DjangoTemplates' Template.
        return self.backend.engine

    def get_exception_info(self, exception, token):
        """Stub matching ``django.template.base.Template.get_exception_info``.

        Django's ``NodeList.render`` in DEBUG mode enriches any exception
        raised by a child node by calling
        ``context.render_context.template.get_exception_info(e, token)``.
        Stock Django's ``Template`` builds a rich debug payload (source
        snippet, line numbers). Our adapter doesn't carry that at the
        Python level (the Rust ``Template`` owns the source), so we
        return an empty dict; the debug page renders without the
        offending detail rather than 500'ing.
        """
        return {}

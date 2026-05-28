"""django-template-oxide: a Rust-accelerated Django template engine.

Two ways to use this package:

1. As a TEMPLATES backend: put
   ``"django_template_oxide.backend.OxideTemplates"`` in your ``TEMPLATES``
   setting. The backend installs Rust acceleration on instantiation.

2. As an INSTALLED_APPS hook: add ``"django_template_oxide"`` to
   ``INSTALLED_APPS``. ``AppConfig.ready()`` installs the same
   acceleration without requiring the OxideTemplates backend.

Both paths call :func:`enable_rust_nodelist_acceleration`, which swaps
``django.template.base.NodeList.render`` for a Rust implementation. The
function is idempotent.
"""

from importlib.metadata import PackageNotFoundError, version as _pkg_version

from django_template_oxide._patch import enable_rust_nodelist_acceleration

default_app_config = "django_template_oxide.apps.OxideConfig"

try:
    __version__ = _pkg_version("django-template-oxide")
except PackageNotFoundError:
    # Editable / dev install without recorded metadata: fall back to the
    # version baked into the Rust extension at compile time.
    from django_template_oxide._rust import __version__

__all__ = ["__version__", "enable_rust_nodelist_acceleration"]

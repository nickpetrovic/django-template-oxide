"""Django ``AppConfig`` for django-template-oxide.

Loading this app via ``INSTALLED_APPS`` opts in to global Rust
acceleration of ``django.template.base.NodeList.render``. The patching
is delegated to :mod:`django_template_oxide._patch` so the same code
path is reusable by :class:`OxideTemplates` and direct callers.

For scoped activation (only when the OxideTemplates backend is
constructed), omit this app from ``INSTALLED_APPS`` and configure
``TEMPLATES`` with ``OxideTemplates`` as the backend.
"""

from django.apps import AppConfig


class OxideConfig(AppConfig):
    name = "django_template_oxide"
    verbose_name = "Template Oxide Engine"
    default_auto_field = "django.db.models.BigAutoField"

    def ready(self) -> None:
        from django_template_oxide._patch import enable_rust_nodelist_acceleration

        enable_rust_nodelist_acceleration()

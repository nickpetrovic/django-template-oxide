"""Alias path that lets third-party libraries auto-detect oxide.

Several Django libraries auto-wire themselves into a specific
``TEMPLATES`` entry at ``AppConfig.ready()`` time by iterating
``settings.TEMPLATES`` and matching the entry by name. The match logic
is the same shape in every library that does this (django-cotton,
django-allauth, debug_toolbar, etc.)::

    name = entry.get("NAME")
    if not name:
        name = entry["BACKEND"].rsplit(".", 2)[-2]
    if name == "django":
        # inject loaders / builtins / autodiscovery into this entry

When ``NAME`` isn't set, the fallback uses the penultimate dotted
segment of the ``BACKEND`` path. For stock Django that's ``"django"``.

The canonical oxide ``BACKEND`` path is
``django_template_oxide.backend.OxideTemplates`` whose penultimate
segment is ``"backend"``, so libraries doing the above match skip the
oxide entry. The classic workaround is adding ``"NAME": "django"``.

This module removes the workaround. Configuring::

    TEMPLATES = [
        {
            "BACKEND": "django_template_oxide.backends.django.OxideTemplates",
            "DIRS": [...],
            "APP_DIRS": True,
            "OPTIONS": {...},
        },
    ]

makes ``BACKEND.rsplit(".", 2)[-2]`` evaluate to ``"django"``, so every
library following the convention finds the entry without an explicit
``NAME`` hint. Pure alias, no behaviour duplicated.
"""

from django_template_oxide.backend import OxideTemplates

__all__ = ["OxideTemplates"]

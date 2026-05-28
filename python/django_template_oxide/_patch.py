"""Runtime patch that swaps ``django.template.base.NodeList.render`` for
a Rust implementation.

Factored out of ``apps.py`` so the same logic is reusable from
``OxideConfig.ready()``, ``OxideTemplates.__init__()``, and direct
callers (tests).

The patch is idempotent and reversible: the original ``NodeList.render``
is stored on the class as ``NodeList._oxide_original_render``.
"""

from __future__ import annotations

import threading

_LOCK = threading.Lock()
_PATCHED = False


def enable_rust_nodelist_acceleration() -> None:
    """Install the Rust-backed ``NodeList.render`` if not already installed.

    Thread-safe and idempotent.
    """
    global _PATCHED
    if _PATCHED:
        return
    with _LOCK:
        if _PATCHED:
            return
        _install_patch()
        _PATCHED = True


def disable_rust_nodelist_acceleration() -> None:
    """Restore Django's original ``NodeList.render``. Intended for tests."""
    global _PATCHED
    with _LOCK:
        if not _PATCHED:
            return
        from django.template.base import NodeList

        original = getattr(NodeList, "_oxide_original_render", None)
        if original is not None:
            NodeList.render = original
            del NodeList._oxide_original_render
        _PATCHED = False


def is_enabled() -> bool:
    """Whether the Rust patch is currently active."""
    return _PATCHED


def _install_patch() -> None:
    """Internal. Callers go through :func:`enable_rust_nodelist_acceleration`."""

    from django.template.base import NodeList, TextNode, VariableNode
    from django.utils.safestring import mark_safe

    from django_template_oxide._rust import render_nodelist

    # Stash the original so disable_rust_nodelist_acceleration() can restore it.
    NodeList._oxide_original_render = NodeList.render

    def _oxide_nodelist_render(self, context):
        return mark_safe(render_nodelist(self, context, TextNode, VariableNode))

    NodeList.render = _oxide_nodelist_render

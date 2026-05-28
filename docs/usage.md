# Using

## Wire it up in settings.py

```python
TEMPLATES = [
    {
        "BACKEND": "django_template_oxide.backend.OxideTemplates",
        "DIRS": [BASE_DIR / "templates"],
        "APP_DIRS": True,
        "OPTIONS": {
            "context_processors": [
                "django.template.context_processors.request",
                "django.contrib.auth.context_processors.auth",
                "django.contrib.messages.context_processors.messages",
            ],
            # Anything else you'd pass to DjangoTemplates OPTIONS works
            # here too: `libraries`, `builtins`, `loaders`, `debug`,
            # `string_if_invalid`, `autoescape`.
        },
    },
]
```

That's it. The rest of Django (views, `render()` shortcut, class-based
views, `get_template`, `render_to_string`) all keep working.

## What changes for your views

Nothing.

```python
from django.shortcuts import render

def dashboard(request):
    return render(request, "pages/dashboard.html", {"user": request.user})
```

Reads the template via oxide's loader, compiles via the Rust engine,
renders via the Rust engine. Output is byte-identical to what stock
Django would have produced.

## Custom tag libraries

Your existing `templatetags/` modules keep working unchanged. Oxide
honors the standard Django `Library` API:

```python
from django import template

register = template.Library()

@register.simple_tag
def greet(name):
    return f"Hello, {name}!"

@register.filter
def double(value):
    return value * 2

@register.tag
def my_block(parser, token):
    nodelist = parser.parse(("endmyblock",))
    parser.delete_first_token()
    return MyBlockNode(nodelist)
```

The `{% load %}` tag finds them via the standard Django mechanism.
The Python `Node.render(context)` runs through oxide's Python-Node
adapter; context mutations propagate back into the surrounding
Rust render correctly.

## Switching back

Change the `BACKEND` line back to
`django.template.backends.django.DjangoTemplates`. No other change
needed. We don't fork Django; we plug into it.

# Custom Tags and Filters

Oxide honors the full Django `Library` API. Anything documented in
[Django's custom template tags howto](https://docs.djangoproject.com/en/6.0/howto/custom-template-tags/)
works here unchanged.

## Quick reference

```python
# myapp/templatetags/myapp_tags.py
from django import template

register = template.Library()
```

### `@register.filter`

```python
@register.filter
def double(value):
    return value * 2

@register.filter(name="dub", is_safe=True)
def double_safe(value):
    return value * 2
```

### `@register.simple_tag`

```python
@register.simple_tag
def hello(name):
    return f"Hello, {name}!"

@register.simple_tag(takes_context=True)
def hello_user(context):
    return f"Hello, {context['request'].user}"

@register.simple_tag(name="hello")
def hello_renamed(name):
    return f"Hi, {name}!"
```

### `@register.simple_block_tag`

```python
@register.simple_block_tag
def upper(content):
    return content.upper()

@register.simple_block_tag(end_name="endbold")
def bold(content):
    return f"<b>{content}</b>"
```

### `@register.inclusion_tag`

```python
@register.inclusion_tag("snippets/btn.html")
def button(label, url):
    return {"label": label, "url": url}
```

### Raw `@register.tag`

For when you need full control over parser interaction:

```python
@register.tag(name="upper")
def do_upper(parser, token):
    nodelist = parser.parse(("endupper",))
    parser.delete_first_token()
    return UpperNode(nodelist)


class UpperNode(template.Node):
    def __init__(self, nodelist):
        self.nodelist = nodelist

    def render(self, context):
        return self.nodelist.render(context).upper()
```

The `parser` and `token` arguments expose the same surface stock
Django gives you:

- `parser.parse(parse_until)`, parses until one of the listed
  end-tokens; returns a `NodeList`.
- `parser.delete_first_token()`, consume the closing tag.
- `parser.compile_filter(token_string)`, parse a `var|filter:arg`
  expression into a `FilterExpression` you can `.resolve(context)`.
- `parser.error(token, msg)`, build a `TemplateSyntaxError`
  annotated with the token.
- `parser.extra_data`, scratchpad dict for cross-tag state.
- `parser.tags`, `parser.filters`, `parser.origin`, the standard
  introspection attributes.
- `token.split_contents()`, `token.contents`, `token.lineno`,
  `token.token_type`, all there.

## Loading the library

```html
{% load myapp_tags %}
```

works as in stock Django. Tag/filter resolution goes through the
standard `Library` import path.

## What happens under the hood

Your `Node.render(context)` is called by oxide's render path with a
`PyContext` wrapper around the Rust `Context`. Mutations to the
context (e.g. `context["x"] = 1` inside your render) propagate back
into the surrounding Rust render, matching Django's mutable-context
semantics.

The cost of calling Python from Rust on each render is around a
microsecond on warm caches. For tags that render frequently (per
loop iteration, for example), prefer to do work in the compile step
and keep `render()` cheap.

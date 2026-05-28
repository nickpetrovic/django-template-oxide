"""Raw `@register.tag` fixture library exercising every documented API
of the low-level (parser, token) -> Node compilation path.

Every tag is registered via `register.tag(...)` directly (no
`simple_tag` / `simple_block_tag` / `inclusion_tag` shortcuts). The
compile functions touch every Parser / Token method documented in:

  - django/template/library.py (Library.tag, lines 29-55)
  - django/template/base.py (Parser, Token, Node, NodeList, FilterExpression, Variable)
  - https://docs.djangoproject.com/en/6.0/howto/custom-template-tags/

Tags expose internals (e.g. `LAST_*` module globals) so tests can
assert specific Parser methods were called with specific arguments.

Loaded as the `raw` alias from `tests/test_regressions.py` (see
`_TEST_LIBRARIES`).
"""

from django import template
from django.template.base import (
    Node,
    NodeList,
    TextNode,
    Token,
    TokenType,
    VariableDoesNotExist,
    token_kwargs,
)
from django.template.exceptions import TemplateSyntaxError
from django.utils.safestring import mark_safe

# Tests reach this Library via `{% load raw %}`.
register = template.Library()


# A. Library.tag registration: every documented decorator form.
# Each tag is registered through a different branch of
# `Library.tag(name=None, compile_function=None)` to verify every code
# path in library.py:29-55.


class _LiteralNode(Node):
    """Render a fixed string."""

    def __init__(self, literal):
        self.literal = literal

    def render(self, context):
        return self.literal


def _make_literal_compile(literal):
    def compile_func(parser, token):
        return _LiteralNode(literal)

    return compile_func


# Form 1: `@register.tag` (bare; library.py:34-36, library.py:53-55).
@register.tag
def form_bare(parser, token):
    return _LiteralNode("form_bare")


# Form 2: `@register.tag()` (library.py:30-32).
@register.tag()
def form_paren(parser, token):
    return _LiteralNode("form_paren")


# Form 3: `@register.tag('name')` (library.py:37-42).
@register.tag("form_pos_name")
def _form_pos_impl(parser, token):
    return _LiteralNode("form_pos")


# Form 4: `@register.tag(name='name')` (library.py:37-42).
@register.tag(name="form_kw_name")
def _form_kw_impl(parser, token):
    return _LiteralNode("form_kw")


# Form 5: `register.tag('name', func)` (library.py:43-46).
register.tag("form_call", _make_literal_compile("form_call"))


# Form 6: `register.tag_function(func)` (library.py:53-55).
def _form_tag_function_impl(parser, token):
    return _LiteralNode("form_tag_function")


register.tag_function(_form_tag_function_impl)


# B. Token contract: compile fn pokes every attribute of the Token.

TOKEN_RECORD = {
    "contents": None,
    "split_contents": None,
    "lineno": None,
    "position": None,
    "token_type_name": None,
    "repr": None,
    "first_bit": None,
}


@register.tag(name="record_token")
def do_record_token(parser, token):
    """Capture every documented Token field for the test to inspect:
    contents, split_contents(), lineno, position, token_type, repr,
    and the `token.contents.split()[0]` idiom."""
    TOKEN_RECORD["contents"] = token.contents
    TOKEN_RECORD["split_contents"] = token.split_contents()
    TOKEN_RECORD["lineno"] = token.lineno
    TOKEN_RECORD["position"] = token.position
    TOKEN_RECORD["token_type_name"] = token.token_type.name
    TOKEN_RECORD["repr"] = repr(token)
    TOKEN_RECORD["first_bit"] = token.contents.split()[0]
    return _LiteralNode("")


@register.tag(name="echo_split")
def do_echo_split(parser, token):
    """Echo `' | '.join(token.split_contents())`. Tests that quoted
    strings and `_("...")` markers are preserved by `split_contents()`."""
    bits = token.split_contents()
    return _LiteralNode(" | ".join(bits))


@register.tag(name="echo_token_type")
def do_echo_token_type(parser, token):
    """Proves the PyToken proxy returns the real TokenType enum
    (identity-comparable to `TokenType.BLOCK`)."""
    name = token.token_type.name
    is_block = token.token_type is TokenType.BLOCK
    return _LiteralNode(f"{name}/{is_block}")


# C. Compile function returning non-Node / raising.


@register.tag(name="raise_syntax")
def do_raise_syntax(parser, token):
    """Per base.py:582-585, the exception is annotated with the token
    by `parser.error(token, e)` and re-raised."""
    raise TemplateSyntaxError("forced-syntax-error from raise_syntax")


@register.tag(name="raise_generic")
def do_raise_generic(parser, token):
    """Django's `parser.error` wraps generic exceptions (base.py:626-630)."""
    raise RuntimeError("forced-runtime-error from raise_generic")


@register.tag(name="return_none")
def do_return_none(parser, token):
    """Compile function returning `None`. Django stores `None` in the
    nodelist and crashes at render time when `None.render_annotated`
    is invoked. We assert the shape, not message text."""
    return None


# D. Parser API: every method exposed to compile functions.


class _UpperNode(Node):
    def __init__(self, nodelist):
        self.nodelist = nodelist

    def render(self, context):
        return self.nodelist.render(context).upper()


@register.tag(name="upper")
def do_upper(parser, token):
    """Block tag exactly like Django's `{% upper %}` doc example."""
    nodelist = parser.parse(("endupper",))
    parser.delete_first_token()
    return _UpperNode(nodelist)


@register.tag(name="upper_list_terminator")
def do_upper_list_terminator(parser, token):
    """Same as `upper` but `parse_until` is a LIST instead of tuple."""
    nodelist = parser.parse(["endupper_list"])
    parser.delete_first_token()
    return _UpperNode(nodelist)


@register.tag(name="upper_multi_terminator")
def do_upper_multi_terminator(parser, token):
    """Multiple terminators. Exercises the `command in parse_until`
    branch in base.py:564."""
    nodelist = parser.parse(("endupper_a", "endupper_b"))
    parser.delete_first_token()
    return _UpperNode(nodelist)


@register.tag(name="rest_of_template_lower")
def do_rest_of_template_lower(parser, token):
    """`parser.parse()` with no arg consumes to EOF. Exercises
    `parse(None)`/`parse([])`."""
    nodelist = parser.parse()
    return _LowerNode(nodelist)


class _LowerNode(Node):
    def __init__(self, nodelist):
        self.nodelist = nodelist

    def render(self, context):
        return self.nodelist.render(context).lower()


class _NullNode(Node):
    def render(self, context):
        return ""


@register.tag(name="skip_block")
def do_skip_block(parser, token):
    """`parser.skip_past('endskip')`. Mirrors `{% comment %}`
    (base.py:593-598)."""
    parser.skip_past("endskip")
    return _NullNode()


@register.tag(name="peek_then_consume")
def do_peek_then_consume(parser, token):
    """Exercises `parser.next_token()` and `parser.prepend_token()`.
    Pops the next token, inspects, prepends, parses normally."""
    peeked = parser.next_token()
    captured_contents = peeked.contents
    parser.prepend_token(peeked)
    nodelist = parser.parse(("endpeek",))
    parser.delete_first_token()
    return _PeekNode(captured_contents, nodelist)


class _PeekNode(Node):
    def __init__(self, captured, nodelist):
        self.captured = captured
        self.nodelist = nodelist

    def render(self, context):
        return f"[peeked:{self.captured}]{self.nodelist.render(context)}"


class _ResolveFilterNode(Node):
    def __init__(self, fe):
        self.fe = fe

    def render(self, context):
        return str(self.fe.resolve(context))


@register.tag(name="resolve_filter")
def do_resolve_filter(parser, token):
    """Exercises `parser.compile_filter`."""
    _, expr = token.contents.split(None, 1)
    fe = parser.compile_filter(expr)
    return _ResolveFilterNode(fe)


@register.tag(name="resolve_filter_ignoring_failures")
def do_resolve_filter_ignore(parser, token):
    """`ignore_failures=True`: missing variable yields None, not
    `string_if_invalid`."""
    _, expr = token.contents.split(None, 1)
    fe = parser.compile_filter(expr)
    return _ResolveFilterIgnoreNode(fe)


class _ResolveFilterIgnoreNode(Node):
    def __init__(self, fe):
        self.fe = fe

    def render(self, context):
        val = self.fe.resolve(context, ignore_failures=True)
        return "NONE" if val is None else str(val)


@register.tag(name="parser_error_str")
def do_parser_error_str(parser, token):
    """`raise parser.error(token, "...")` from base.py docstrings.
    Exercises string -> TemplateSyntaxError conversion."""
    raise parser.error(token, "string-message via parser.error")


@register.tag(name="parser_error_exc")
def do_parser_error_exc(parser, token):
    """Passing an Exception instance: Django returns it unchanged with
    `.token` attached (base.py:626-629)."""
    exc = TemplateSyntaxError("exc-instance via parser.error")
    raise parser.error(token, exc)


PARSER_TAGS_SNAPSHOT = {"keys": None, "has_upper": None}


@register.tag(name="snapshot_parser_tags")
def do_snapshot_parser_tags(parser, token):
    """Record `parser.tags` keys at compile time."""
    PARSER_TAGS_SNAPSHOT["keys"] = sorted(parser.tags.keys())
    PARSER_TAGS_SNAPSHOT["has_upper"] = "upper" in parser.tags
    return _LiteralNode("")


PARSER_FILTERS_SNAPSHOT = {"keys": None}


@register.tag(name="snapshot_parser_filters")
def do_snapshot_parser_filters(parser, token):
    PARSER_FILTERS_SNAPSHOT["keys"] = sorted(parser.filters.keys())
    return _LiteralNode("")


PARSER_ORIGIN_SNAPSHOT = {"name": None, "template_name": None, "is_none": None}


@register.tag(name="snapshot_parser_origin")
def do_snapshot_parser_origin(parser, token):
    origin = parser.origin
    PARSER_ORIGIN_SNAPSHOT["is_none"] = origin is None
    if origin is not None:
        PARSER_ORIGIN_SNAPSHOT["name"] = origin.name
        PARSER_ORIGIN_SNAPSHOT["template_name"] = origin.template_name
    return _LiteralNode("")


# `parser.extra_data` is exposed by Django 5.2+ for cross-tag state
# (used by template-partials).
@register.tag(name="extra_data_set")
def do_extra_data_set(parser, token):
    parser.extra_data["raw_tags_marker"] = "set-by-extra_data_set"
    return _LiteralNode("")


@register.tag(name="extra_data_get")
def do_extra_data_get(parser, token):
    val = parser.extra_data.get("raw_tags_marker", "absent")
    return _LiteralNode(str(val))


# E. Node.render contract: setting vars, render_annotated.


class _SetVarNode(Node):
    """Set a context variable; render empty."""

    def __init__(self, var_name, value):
        self.var_name = var_name
        self.value = value

    def render(self, context):
        context[self.var_name] = self.value
        return ""


@register.tag(name="set_var")
def do_set_var(parser, token):
    """`{% set_var name value %}` writes `value` (literal string) into
    the context under `name`."""
    bits = token.split_contents()
    if len(bits) != 3:
        raise template.TemplateSyntaxError(
            "%r tag requires exactly two arguments" % bits[0]
        )
    _, name, value = bits
    if value[0] == value[-1] and value[0] in ('"', "'"):
        value = value[1:-1]
    return _SetVarNode(name, value)


class _RenderAnnotatedOnlyNode(Node):
    """Defines `render_annotated` but not `render`. Oxide must call the
    annotated method when overridden directly."""

    def render_annotated(self, context):
        return "annotated-path"


@register.tag(name="render_annotated_only")
def do_render_annotated_only(parser, token):
    return _RenderAnnotatedOnlyNode()


class _CounterNode(Node):
    """Increments a counter on `context.render_context`. Verifies (a)
    `render()` is called once per render pass and (b) `render_context[self]`
    is the correct thread-safety idiom."""

    def render(self, context):
        rc = context.render_context
        if self not in rc:
            rc[self] = 0
        rc[self] += 1
        return str(rc[self])


@register.tag(name="counter")
def do_counter(parser, token):
    return _CounterNode()


class _DebugInspectNode(Node):
    def render(self, context):
        return str(context.template.engine.debug)


@register.tag(name="debug_inspect")
def do_debug_inspect(parser, token):
    return _DebugInspectNode()


class _AutoescapeInspectNode(Node):
    def render(self, context):
        return str(context.autoescape)


@register.tag(name="autoescape_inspect")
def do_autoescape_inspect(parser, token):
    return _AutoescapeInspectNode()


class _MarkSafeHtmlNode(Node):
    def render(self, context):
        return mark_safe("<b>safe-html</b>")


@register.tag(name="mark_safe_html")
def do_mark_safe_html(parser, token):
    return _MarkSafeHtmlNode()


# Raw HTML (not marked safe); autoescape should escape it.
class _RawHtmlNode(Node):
    def render(self, context):
        return "<b>raw-html</b>"


@register.tag(name="raw_html")
def do_raw_html(parser, token):
    return _RawHtmlNode()


# F. Block-tag patterns: parse_until + saved nodelist.


@register.tag(name="reverse")
def do_reverse(parser, token):
    """Reverse the rendered body; tests that the body sees the same
    context with mutations visible."""
    nodelist = parser.parse(("endreverse",))
    parser.delete_first_token()
    return _ReverseNode(nodelist)


class _ReverseNode(Node):
    def __init__(self, nodelist):
        self.nodelist = nodelist

    def render(self, context):
        return self.nodelist.render(context)[::-1]


# G. Variable resolution.


class _FormatTimeNode(Node):
    """Verbatim from the "Passing template variables to the tag" docs.
    Demonstrates `template.Variable(...).resolve(context)` and the
    `VariableDoesNotExist` catch path."""

    def __init__(self, var_name, fallback):
        self.var = template.Variable(var_name)
        self.fallback = fallback

    def render(self, context):
        try:
            return str(self.var.resolve(context))
        except VariableDoesNotExist:
            return self.fallback


@register.tag(name="resolve_var")
def do_resolve_var(parser, token):
    """`{% resolve_var name fallback %}`: resolve `name`; on failure
    render the literal `fallback`."""
    bits = token.split_contents()
    if len(bits) != 3:
        raise template.TemplateSyntaxError("usage: resolve_var name fallback")
    _, name, fallback = bits
    if fallback[0] == fallback[-1] and fallback[0] in ('"', "'"):
        fallback = fallback[1:-1]
    return _FormatTimeNode(name, fallback)


# K. `token_kwargs` helper.


class _KwargsEchoNode(Node):
    def __init__(self, kwargs):
        self.kwargs = kwargs

    def render(self, context):
        parts = []
        for k in sorted(self.kwargs):
            val = self.kwargs[k].resolve(context)
            parts.append(f"{k}={val}")
        return " ".join(parts)


@register.tag(name="kwargs_echo")
def do_kwargs_echo(parser, token):
    """Exercises `token_kwargs(bits, parser)` from third-party tags."""
    bits = token.split_contents()[1:]
    kwargs = token_kwargs(bits, parser)
    if not kwargs and bits:
        raise template.TemplateSyntaxError(
            "kwargs_echo: could not parse %r as key=value pairs" % bits
        )
    return _KwargsEchoNode(kwargs)


@register.tag(name="kwargs_partial")
def do_kwargs_partial(parser, token):
    """token_kwargs should stop at the first non-kwarg bit and leave
    the rest in `bits`."""
    bits = token.split_contents()[1:]
    kwargs = token_kwargs(bits, parser)
    return _KwargsPartialNode(kwargs, len(bits))


class _KwargsPartialNode(Node):
    def __init__(self, kwargs, leftover_count):
        self.kwargs = kwargs
        self.leftover_count = leftover_count

    def render(self, context):
        parts = []
        for k in sorted(self.kwargs):
            parts.append(f"{k}={self.kwargs[k].resolve(context)}")
        return " ".join(parts) + f" leftover={self.leftover_count}"


# L. FilterExpression introspection. Django's `FilterExpression`
# exposes `.var`, `.filters`, `.is_var`, `str(fe) == token`. Used by
# django-cotton, django-debug-toolbar.


class _FeIntrospectNode(Node):
    def __init__(self, fe):
        self.fe = fe

    def render(self, context):
        parts = [
            f"is_var={self.fe.is_var}",
            f"str={str(self.fe)}",
            f"n_filters={len(self.fe.filters)}",
            f"var_type={type(self.fe.var).__name__}",
        ]
        return ";".join(parts)


@register.tag(name="fe_introspect")
def do_fe_introspect(parser, token):
    """Emit `.is_var`, `str(fe)`, `len(fe.filters)`, `type(fe.var).__name__`."""
    _, expr = token.contents.split(None, 1)
    fe = parser.compile_filter(expr)
    return _FeIntrospectNode(fe)


# M. NodeList introspection: len, iter, get_nodes_by_type, contains_nontext.


class _NlIntrospectNode(Node):
    def __init__(self, nodelist):
        self.nodelist = nodelist

    def render(self, context):
        text_nodes = self.nodelist.get_nodes_by_type(TextNode)
        all_types = [type(n).__name__ for n in self.nodelist]
        parts = [
            f"len={len(self.nodelist)}",
            f"text_n={len(text_nodes)}",
            f"types={','.join(all_types)}",
            f"contains_nontext={self.nodelist.contains_nontext}",
        ]
        return ";".join(parts)


@register.tag(name="nl_introspect")
def do_nl_introspect(parser, token):
    """Parse to `{% endnl %}`; emit NodeList introspection."""
    nodelist = parser.parse(("endnl",))
    parser.delete_first_token()
    return _NlIntrospectNode(nodelist)


# N. Edge cases.


@register.tag(name="no_args")
def do_no_args(parser, token):
    if token.contents != "no_args":
        raise template.TemplateSyntaxError(
            "no_args: expected contents='no_args', got %r" % token.contents
        )
    return _LiteralNode("ok")


@register.tag(name="trans_marker_echo")
def do_trans_marker_echo(parser, token):
    """Echo the second bit (a translation marker like `_("Hello")`
    preserved verbatim by `split_contents`)."""
    bits = token.split_contents()
    return _LiteralNode(bits[1] if len(bits) >= 2 else "")


# Returns an integer to prove Django/oxide both crash with the same
# shape; contract test, not caught.
class _IntReturnNode(Node):
    def render(self, context):
        return 42


@register.tag(name="int_return")
def do_int_return(parser, token):
    return _IntReturnNode()

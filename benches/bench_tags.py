from django import template

register = template.Library()


@register.filter
def bench_noop(value, _arg=None):
    return value


@register.simple_tag
def bench_simple_tag(value):
    return f"[{value}]"


class _RawTagNode(template.Node):
    def __init__(self, var):
        self.var = var

    def render(self, context):
        return f"<{self.var.resolve(context)}>"


@register.tag(name="bench_raw_tag")
def bench_raw_tag(parser, token):
    bits = token.split_contents()
    if len(bits) != 2:
        raise template.TemplateSyntaxError("bench_raw_tag takes one argument")
    return _RawTagNode(parser.compile_filter(bits[1]))

from django import template

register = template.Library()


@register.filter
def upper(value):
    return f"UPPER({value})"


@register.filter
def length(value):
    return "LENGTH"


@register.filter
def first(value):
    return "FIRST"


@register.filter(is_safe=True)
def default(value, arg):
    return f"DEFAULT({value},{arg})"

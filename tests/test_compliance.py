"""Compliance tests: verify django-template-oxide produces identical
output to Django's built-in template engine for every test case.

Does NOT monkey-patch Django; renders each template through BOTH
engines and asserts outputs match.
"""

import os
import sys

import pytest

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")
sys.path.insert(0, os.path.dirname(__file__))

import django

django.setup()

from django.template import Template as DjangoTemplate, Context as DjangoContext
from django_template_oxide._rust import Template as OxideTemplate, Context as OxideContext


def render_both(template_string, context_dict):
    """Render through both engines; return (django_result, oxide_result)."""
    django_result = DjangoTemplate(template_string).render(DjangoContext(context_dict))
    oxide_result = OxideTemplate(template_string).render(OxideContext(context_dict))
    return django_result, oxide_result


class TestVariableCompliance:
    """Variable resolution must match Django exactly."""



    @pytest.mark.parametrize(
        "template,context",
        [
            ("{{ x }}", {"x": "hello"}),
            ("{{ x }}", {"x": 42}),
            ("{{ x }}", {"x": 3.14}),
            ("{{ x }}", {"x": True}),
            ("{{ x }}", {"x": False}),
            ("{{ x }}", {"x": None}),
            ("{{ x }}", {"x": ""}),
            ("{{ x }}", {"x": "<b>html</b>"}),
            ("{{ x }}", {}),
            ("{{ x.y }}", {"x": {"y": "nested"}}),
            ("{{ x.0 }}", {"x": ["first", "second"]}),
            ("{{ x.1 }}", {"x": ["first", "second"]}),
        ],
    )
    def test_variable_output(self, template, context):
        django_out, oxide_out = render_both(template, context)
        assert oxide_out == django_out, (
            f"Template: {template!r}, Context: {context!r}\n"
            f"  Django: {django_out!r}\n"
            f"  Oxide:  {oxide_out!r}"
        )


class TestFilterCompliance:
    """Filters must produce identical output to Django."""

    @pytest.mark.parametrize(
        "template,context",
        [
            ('{{ x|lower }}', {"x": "HELLO"}),
            ('{{ x|upper }}', {"x": "hello"}),
            ('{{ x|capfirst }}', {"x": "hello"}),
            ('{{ x|title }}', {"x": "hello world"}),
            ('{{ x|length }}', {"x": "hello"}),
            ('{{ x|length }}', {"x": [1, 2, 3]}),
            ('{{ x|default:"fallback" }}', {"x": ""}),
            ('{{ x|default:"fallback" }}', {"x": "present"}),
            ('{{ x|default_if_none:"fallback" }}', {"x": None}),
            ('{{ x|default_if_none:"fallback" }}', {"x": ""}),
            ('{{ x|first }}', {"x": [1, 2, 3]}),
            ('{{ x|last }}', {"x": [1, 2, 3]}),
            ('{{ x|join:", " }}', {"x": ["a", "b", "c"]}),
            ('{{ x|cut:" " }}', {"x": "hello world"}),
            ('{{ x|slugify }}', {"x": "Hello World!"}),
            ('{{ x|truncatechars:5 }}', {"x": "hello world"}),
            ('{{ x|truncatewords:2 }}', {"x": "one two three four"}),
            ('{{ x|add:"5" }}', {"x": 10}),
            ('{{ x|add:"5" }}', {"x": "hello"}),
            ('{{ x|divisibleby:"3" }}', {"x": 9}),
            ('{{ x|divisibleby:"3" }}', {"x": 10}),
            ('{{ x|floatformat }}', {"x": 1.5}),
            ('{{ x|floatformat:"2" }}', {"x": 1.5}),
            ('{{ x|wordcount }}', {"x": "one two three"}),
            ('{{ x|yesno:"yes,no,maybe" }}', {"x": True}),
            ('{{ x|yesno:"yes,no,maybe" }}', {"x": False}),
            ('{{ x|yesno:"yes,no,maybe" }}', {"x": None}),
            ('{{ x|safe }}', {"x": "<b>bold</b>"}),
            ('{{ x|escape }}', {"x": "<b>bold</b>"}),
            ('{{ x|linebreaksbr }}', {"x": "line1\nline2"}),
            ('{{ x|striptags }}', {"x": "<b>hello</b> <i>world</i>"}),
            ('{{ x|addslashes }}', {"x": "it's a \"test\""}),
            ('{{ x|urlencode }}', {"x": "hello world&foo=bar"}),
            ('{{ x|filesizeformat }}', {"x": 1024}),
            ('{{ x|filesizeformat }}', {"x": 1048576}),
            ('{{ x|pluralize }}', {"x": 1}),
            ('{{ x|pluralize }}', {"x": 2}),
            ('{{ x|pluralize:"es" }}', {"x": 1}),
            ('{{ x|pluralize:"es" }}', {"x": 2}),
        ],
    )
    def test_filter_output(self, template, context):
        django_out, oxide_out = render_both(template, context)
        assert oxide_out == django_out, (
            f"Template: {template!r}, Context: {context!r}\n"
            f"  Django: {django_out!r}\n"
            f"  Oxide:  {oxide_out!r}"
        )


class TestTagCompliance:
    """Tags must produce identical output to Django."""

    @pytest.mark.parametrize(
        "template,context",
        [
            ("{% if x %}yes{% endif %}", {"x": True}),
            ("{% if x %}yes{% endif %}", {"x": False}),
            ("{% if x %}yes{% else %}no{% endif %}", {"x": True}),
            ("{% if x %}yes{% else %}no{% endif %}", {"x": False}),
            ("{% if x == 1 %}one{% elif x == 2 %}two{% else %}other{% endif %}", {"x": 1}),
            ("{% if x == 1 %}one{% elif x == 2 %}two{% else %}other{% endif %}", {"x": 2}),
            ("{% if x == 1 %}one{% elif x == 2 %}two{% else %}other{% endif %}", {"x": 3}),
            ("{% if not x %}yes{% endif %}", {"x": False}),
            ("{% if x and y %}yes{% endif %}", {"x": True, "y": True}),
            ("{% if x and y %}yes{% endif %}", {"x": True, "y": False}),
            ("{% if x or y %}yes{% endif %}", {"x": False, "y": True}),
            ("{% if x or y %}yes{% endif %}", {"x": False, "y": False}),
            ("{% for i in items %}{{ i }}{% endfor %}", {"items": [1, 2, 3]}),
            ("{% for i in items %}{{ i }}{% empty %}none{% endfor %}", {"items": []}),
            ("{% for i in items %}{{ forloop.counter }}{% endfor %}", {"items": ["a", "b", "c"]}),
            ("{% for i in items %}{{ forloop.counter0 }}{% endfor %}", {"items": ["a", "b", "c"]}),
            ("{% for i in items %}{{ forloop.first }}{% endfor %}", {"items": ["a", "b"]}),
            ("{% for i in items %}{{ forloop.last }}{% endfor %}", {"items": ["a", "b"]}),
            ("{% for i in items reversed %}{{ i }}{% endfor %}", {"items": [1, 2, 3]}),
            ('{% with x="hello" %}{{ x }}{% endwith %}', {}),
            ('{% with x="hello" y="world" %}{{ x }} {{ y }}{% endwith %}', {}),
            ("before{% comment %}hidden{% endcomment %}after", {}),
            ("{% autoescape off %}{{ x }}{% endautoescape %}", {"x": "<b>hi</b>"}),
            ("{% autoescape on %}{{ x }}{% endautoescape %}", {"x": "<b>hi</b>"}),
            ("{% spaceless %}<b> hi </b>  <i> there </i>{% endspaceless %}", {}),
            ("{% templatetag openblock %}", {}),
            ("{% templatetag closeblock %}", {}),
            ("{% templatetag openvariable %}", {}),
            ("{% templatetag closevariable %}", {}),
            ("{% firstof a b c %}", {"a": "", "b": "yes", "c": "no"}),
            ("{% firstof a b c %}", {"a": "first", "b": "yes", "c": "no"}),
            ("{% csrf_token %}", {"csrf_token": "abc123"}),
            (
                "{% for i in items %}{% if i %}{{ i }}{% endif %}{% endfor %}",
                {"items": [1, 0, 2, 0, 3]},
            ),
        ],
    )
    def test_tag_output(self, template, context):
        django_out, oxide_out = render_both(template, context)
        assert oxide_out == django_out, (
            f"Template: {template!r}, Context: {context!r}\n"
            f"  Django: {django_out!r}\n"
            f"  Oxide:  {oxide_out!r}"
        )


class TestAutoescapeCompliance:
    """Autoescape semantics must match exactly."""

    @pytest.mark.parametrize(
        "template,context",
        [
            ("{{ x }}", {"x": "<script>alert('xss')</script>"}),
            ("{{ x }}", {"x": 'a"b'}),
            ("{{ x }}", {"x": "a'b"}),
            ("{{ x }}", {"x": "a&b"}),
            ("{% autoescape off %}{{ x }}{% endautoescape %}", {"x": "<b>hi</b>"}),
        ],
    )
    def test_escape_output(self, template, context):
        django_out, oxide_out = render_both(template, context)
        assert oxide_out == django_out, (
            f"Template: {template!r}, Context: {context!r}\n"
            f"  Django: {django_out!r}\n"
            f"  Oxide:  {oxide_out!r}"
        )

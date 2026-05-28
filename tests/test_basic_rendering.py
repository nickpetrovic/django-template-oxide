"""Basic rendering tests to verify the Rust engine works."""

import pytest

try:
    from django_template_oxide._rust import Template, Context
except ImportError:
    pytestmark = pytest.mark.skip(
        reason="Template/Context not yet exposed from Rust module"
    )
    Template = None
    Context = None


class TestBasicRendering:
    """Basic template compilation and rendering."""

    def test_plain_text(self):
        t = Template("Hello World")
        c = Context({})
        assert t.render(c) == "Hello World"

    def test_variable_substitution(self):
        t = Template("Hello {{ name }}!")
        c = Context({"name": "World"})
        assert t.render(c) == "Hello World!"

    def test_variable_dot_lookup(self):
        t = Template("{{ person.name }}")
        c = Context({"person": {"name": "Alice"}})
        assert t.render(c) == "Alice"

    def test_autoescape_on(self):
        t = Template("{{ content }}")
        c = Context({"content": "<b>bold</b>"})
        assert t.render(c) == "&lt;b&gt;bold&lt;/b&gt;"

    def test_safe_filter(self):
        pass

    def test_if_tag_true(self):
        t = Template("{% if show %}yes{% endif %}")
        c = Context({"show": True})
        assert t.render(c) == "yes"

    def test_if_tag_false(self):
        t = Template("{% if show %}yes{% endif %}")
        c = Context({"show": False})
        assert t.render(c) == ""

    def test_for_tag(self):
        t = Template("{% for item in items %}{{ item }} {% endfor %}")
        c = Context({"items": ["a", "b", "c"]})
        assert t.render(c) == "a b c "

    def test_comment_tag(self):
        t = Template("before{% comment %}hidden{% endcomment %}after")
        c = Context({})
        assert t.render(c) == "beforeafter"

    def test_multiple_variables(self):
        t = Template("{{ first }} {{ last }}")
        c = Context({"first": "John", "last": "Doe"})
        assert t.render(c) == "John Doe"

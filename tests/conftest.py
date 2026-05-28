"""Pytest conftest: configure Django for template engine tests."""

import importlib
import importlib.util
import os
import sys

import django


class _AliasImporter:
    """Make ``import template_tests.X`` resolve to ``django_template_tests.X``.

    Django's upstream tests reference ``template_tests.templatetags.custom``
    etc. Our copy lives under ``django_template_tests``. This finder
    transparently redirects any ``template_tests.*`` import to the real
    package so both names work.
    """

    _PREFIX = "template_tests"
    _REAL = "django_template_tests"

    def find_spec(self, fullname, path, target=None):
        if fullname == self._PREFIX or fullname.startswith(self._PREFIX + "."):
            real_name = self._REAL + fullname[len(self._PREFIX):]
            real_spec = importlib.util.find_spec(real_name)
            if real_spec is not None:
                return importlib.util.spec_from_file_location(
                    fullname,
                    real_spec.origin,
                    submodule_search_locations=real_spec.submodule_search_locations,
                )
        return None


def pytest_configure():
    tests_dir = os.path.dirname(os.path.abspath(__file__))
    if tests_dir not in sys.path:
        sys.path.insert(0, tests_dir)

    if not any(isinstance(f, _AliasImporter) for f in sys.meta_path):
        sys.meta_path.insert(0, _AliasImporter())

    os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")
    django.setup()

    from django.test.utils import setup_test_environment

    try:
        setup_test_environment()
    except RuntimeError:
        pass  # Already called

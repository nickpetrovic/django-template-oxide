"""Pytest conftest: configure Django for template engine tests."""

import os
import sys

import django


def pytest_configure():
    tests_dir = os.path.dirname(os.path.abspath(__file__))
    if tests_dir not in sys.path:
        sys.path.insert(0, tests_dir)

    os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")
    django.setup()

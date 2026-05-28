"""Run Django's template tests against django-template-oxide.

Monkey-patches Django's template engine to use our Rust implementation
then runs the test suite. Failures indicate incompatibilities to fix.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")

import django

django.setup()

loader = unittest.TestLoader()
suite = loader.discover(
    "django_template_tests",
    pattern="test_*.py",
    top_level_dir=os.path.dirname(__file__),
)

runner = unittest.TextTestRunner(verbosity=2)
result = runner.run(suite)
sys.exit(0 if result.wasSuccessful() else 1)

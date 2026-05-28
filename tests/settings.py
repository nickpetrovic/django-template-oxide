"""Minimal Django settings for running template tests."""

SECRET_KEY = "test-secret-key"

INSTALLED_APPS = [
    "django.contrib.contenttypes",
    "django.contrib.auth",
    "django.contrib.admin",
    "django.contrib.messages",
    "django.contrib.sessions",
    # Test-support package; its `templates/` dir holds fixtures for
    # loader-dependent tag tests (extends, include, block, partial,
    # partialdef), picked up by `APP_DIRS=True` below.
    "django_template_tests",
]

DATABASES = {
    "default": {
        "ENGINE": "django.db.backends.sqlite3",
        "NAME": ":memory:",
    }
}

TEMPLATES = [
    {
        "BACKEND": "django.template.backends.django.DjangoTemplates",
        "DIRS": [],
        "APP_DIRS": True,
        "OPTIONS": {
            "context_processors": [],
        },
    },
]

USE_TZ = True
USE_I18N = True
USE_L10N = False

# Point to Django's own locale files for translation tests.
import os
import django

LOCALE_PATHS = [
    os.path.join(os.path.dirname(django.__file__), "conf", "locale"),
    os.path.join(os.path.dirname(__file__), "i18n", "other", "locale"),
]

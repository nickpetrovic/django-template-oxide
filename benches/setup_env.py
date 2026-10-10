from pathlib import Path

import django
from django.conf import settings
from django.core.management import call_command
from django.template.backends.django import DjangoTemplates
from django_rusty_templates import RustyTemplates
from django_template_oxide.backend import OxideTemplates

BENCH_DIR = Path(__file__).resolve().parent
TEMPLATE_DIR = BENCH_DIR / "templates"
ENGINE_NAMES = ("oxide", "rusty", "stock")

_BUILTINS = [
    "django.template.defaulttags",
    "django.template.defaultfilters",
    "django.template.loader_tags",
]
_CONTEXT_PROCESSORS = [
    "django.template.context_processors.request",
    "django.contrib.auth.context_processors.auth",
    "django.contrib.messages.context_processors.messages",
]


def engine_options(cotton=False, cached=True):
    loaders = [
        "django.template.loaders.filesystem.Loader",
        "django.template.loaders.app_directories.Loader",
    ]
    builtins = list(_BUILTINS)
    if cotton:
        loaders.insert(0, "django_cotton.cotton_loader.Loader")
        builtins.append("django_cotton.templatetags.cotton")
    if cached:
        loaders = [("django.template.loaders.cached.Loader", loaders)]
    return {
        "DIRS": [str(TEMPLATE_DIR)],
        "APP_DIRS": False,
        "OPTIONS": {
            "context_processors": list(_CONTEXT_PROCESSORS),
            "builtins": builtins,
            "libraries": {"bench": "bench_tags"},
            "loaders": loaders,
        },
    }


def configure(cotton=False):
    if settings.configured:
        return
    installed_apps = [
        "django.contrib.contenttypes",
        "django.contrib.auth",
        "django.contrib.admin",
        "django.contrib.sessions",
        "django.contrib.messages",
        "django.contrib.staticfiles",
    ]
    if cotton:
        installed_apps.append("django_cotton.apps.SimpleAppConfig")
    settings.configure(
        DEBUG=False,
        SECRET_KEY="bench-not-a-secret",
        ALLOWED_HOSTS=["*"],
        INSTALLED_APPS=installed_apps,
        DATABASES={
            "default": {"ENGINE": "django.db.backends.sqlite3", "NAME": ":memory:"}
        },
        DEFAULT_AUTO_FIELD="django.db.models.AutoField",
        TEMPLATES=[
            {
                "NAME": "django",
                "BACKEND": "django.template.backends.django.DjangoTemplates",
                **engine_options(cotton),
            }
        ],
        ROOT_URLCONF="bench_urls",
        STATIC_URL="/static/",
        USE_TZ=True,
        USE_I18N=True,
        LANGUAGE_CODE="en-us",
        PASSWORD_HASHERS=["django.contrib.auth.hashers.MD5PasswordHasher"],
    )
    django.setup()
    call_command("migrate", verbosity=0, interactive=False)


def build_engines(cotton=False, cached=True):
    factories = {
        "oxide": lambda: OxideTemplates(
            {"NAME": "oxide", **engine_options(cotton, cached)}
        ),
        "rusty": lambda: RustyTemplates(
            {"NAME": "rusty", **_rusty_options(cotton, cached)}
        ),
        "stock": lambda: DjangoTemplates(
            {"NAME": "stock", **engine_options(cotton, cached)}
        ),
    }
    engines = {}
    for name in ENGINE_NAMES:
        try:
            engines[name] = factories[name]()
        except Exception as error:
            engines[name] = error
    return engines


def _rusty_options(cotton, cached):
    options = engine_options(cotton, cached)
    options["OPTIONS"].pop("loaders")
    options["APP_DIRS"] = True
    options["OPTIONS"]["debug"] = not cached
    return options

import datetime
from collections.abc import Callable
from dataclasses import dataclass

from django import forms
from django.contrib import admin
from django.contrib.auth.forms import AuthenticationForm
from django.contrib.auth.models import AnonymousUser, Permission, User
from django.contrib.contenttypes.models import ContentType
from django.contrib.messages.storage.fallback import FallbackStorage
from django.contrib.sessions.backends.db import SessionStore
from django.test import RequestFactory
from django.utils import timezone
from django.utils.translation import gettext_lazy


@dataclass(frozen=True)
class Case:
    label: str
    source: str | None = None
    template_name: str | None = None
    context: Callable[[], dict] = dict
    request: Callable[[], object] | None = None
    fresh_context: bool = False


class _Company:
    __slots__ = ("name",)

    def __init__(self, name):
        self.name = name


class _Posting:
    __slots__ = ("company", "title")

    def __init__(self, title, company):
        self.title = title
        self.company = company


class _Candidate:
    __slots__ = ("name",)

    def __init__(self, name):
        self.name = name


class _Stage:
    __slots__ = ("name", "order")

    def __init__(self, name, order):
        self.name = name
        self.order = order


class _F:
    __slots__ = ("f",)

    def __init__(self, value):
        self.f = value


class _E:
    __slots__ = ("e",)

    def __init__(self, value):
        self.e = _F(value)


class _D:
    __slots__ = ("d",)

    def __init__(self, value):
        self.d = _E(value)


class _C:
    __slots__ = ("c",)

    def __init__(self, value):
        self.c = _D(value)


class _B:
    __slots__ = ("b",)

    def __init__(self, value):
        self.b = _C(value)


class _Deep:
    __slots__ = ("a",)

    def __init__(self, value):
        self.a = _B(value)


class _Application:
    __slots__ = (
        "bio",
        "candidate",
        "created_at",
        "deep",
        "html_blob",
        "id",
        "is_archived",
        "meta",
        "posting",
        "rating",
        "stage",
        "status",
        "tags",
    )

    def __init__(self, i):
        self.id = i
        self.candidate = _Candidate(f"Candidate {i}")
        self.posting = _Posting(f"Posting {i % 30}", _Company(f"Company {i % 6}"))
        self.stage = _Stage(f"Stage {i % 5}", i % 5)
        self.status = ("active", "rejected", "withdrawn", "hired")[i % 4]
        self.created_at = datetime.date(2024, 1, 1) + datetime.timedelta(days=i)
        self.is_archived = i % 7 == 0
        self.rating = i % 10 - 5
        self.tags = ["red", "green", "blue"][: i % 3 + 1]
        self.html_blob = (
            "Plain text & some content"
            if i % 2
            else '<script>alert("xss")</script>&copy;'
        )
        self.deep = _Deep(f"deep-{i}")
        self.bio = (
            f"Senior engineer & <designer> #{i} with 10+ years.\n"
            'Built systems "at scale" and shipped often.\n'
            "More at https://example.com/p?q=1&r=2 today.\n"
        ) * 3
        self.meta = {f"k{n}": f"v{i}-{n}" for n in range(1, 5)}


def build_applications(count):
    return [_Application(i) for i in range(count)]


FULL_TEMPLATE = (
    "<table><thead><tr><th>Name</th><th>Job</th><th>Company</th>"
    "<th>Stage</th><th>Date</th><th>Status</th></tr></thead><tbody>\n"
    "{% for app in applications %}"
    '<tr class="row {% if app.is_archived %}archived{% else %}active{% endif %}">'
    "<td>{{ app.candidate.name }}</td>"
    '<td>{{ app.posting.title|default:"-" }}</td>'
    '<td>{{ app.posting.company.name|default:"-" }}</td>'
    "<td>{{ app.stage.name }}</td>"
    '<td>{{ app.created_at|date:"M d, Y" }}</td>'
    '<td class="status-{{ app.status }}">{{ app.status|title }}</td>'
    "</tr>{% endfor %}</tbody></table>"
)


def render_cases(items):
    def apps():
        return {"applications": build_applications(items), "empty_apps": []}

    loop = "{% for app in applications %}"
    sources = [
        ("Text only, no variables", loop + "<tr><td>plain</td></tr>{% endfor %}"),
        (
            "Three attribute lookups",
            loop + "<tr><td>{{ app.candidate.name }}</td><td>{{ app.stage.name }}</td>"
            "<td>{{ app.status }}</td></tr>{% endfor %}",
        ),
        ("Full table (mixed tags and filters)", FULL_TEMPLATE),
        (
            "Deep lookup (six levels)",
            loop + "{{ app.deep.a.b.c.d.e.f }}{% endfor %}",
        ),
        (
            "Dictionary lookups",
            loop + "{{ app.meta.k1 }}{{ app.meta.k2 }}{{ app.meta.k3 }}{% endfor %}",
        ),
        ("List indexing", loop + "{{ app.tags.0 }}{% endfor %}"),
        (
            "Filter chain (six filters)",
            loop + "{{ app.candidate.name|upper|lower|title|truncatechars:20"
            '|default:"x"|safe }}{% endfor %}',
        ),
        (
            "Date filters",
            loop + '{{ app.created_at|date:"Y-m-d" }}|{{ app.created_at|date:"M d" }}|'
            '{{ app.created_at|date:"D" }}{% endfor %}',
        ),
        (
            "if / elif chain",
            loop + "{% if app.rating < -2 %}terrible{% elif app.rating < 0 %}poor"
            "{% elif app.rating == 0 %}neutral{% elif app.rating < 3 %}good"
            "{% else %}excellent{% endif %}{% endfor %}",
        ),
        (
            "Nested {% with %}",
            loop + "{% with n=app.candidate.name %}{% with s=app.stage.name %}"
            "{% with st=app.status %}{% with c=app.posting.company.name %}"
            "{{ n }}|{{ s }}|{{ st }}|{{ c }}"
            "{% endwith %}{% endwith %}{% endwith %}{% endwith %}{% endfor %}",
        ),
        (
            "forloop variables",
            loop + "{{ forloop.counter }}:{{ app.candidate.name }}"
            "{% if forloop.first %}[first]{% endif %}"
            "{% if forloop.last %}[last]{% endif %}{% endfor %}",
        ),
        (
            "{% cycle %}",
            loop + "<tr class=\"{% cycle 'odd' 'even' 'other' %}\">"
            "{{ app.candidate.name }}</tr>{% endfor %}",
        ),
        ("Autoescaping HTML", loop + "<div>{{ app.html_blob }}</div>{% endfor %}"),
        (
            "Nested loops",
            loop + "{% for t in app.tags %}{{ forloop.counter }}:{{ t }};{% endfor %}"
            "{% endfor %}",
        ),
        (
            "Boolean {% if %} (and, or, not, in)",
            loop
            + "{% if app.rating > 0 and 'red' in app.tags and not app.is_archived %}"
            "Y{% else %}N{% endif %}{% endfor %}",
        ),
        (
            "{% translate %} per row",
            "{% load i18n %}" + loop + "{% translate 'Status' %}:{{ app.status }} "
            "{% endfor %}",
        ),
        ("Long text autoescaping", loop + "<p>{{ app.bio }}</p>{% endfor %}"),
        (
            "Text filters (truncatewords, linebreaksbr)",
            loop + "{{ app.bio|truncatewords:20|linebreaksbr }}{% endfor %}",
        ),
        (
            "{% url %} per row",
            loop + "<a href=\"{% url 'detail' app.id %}\">{{ app.candidate.name }}</a>"
            "{% endfor %}",
        ),
        (
            "{% csrf_token %} per row",
            loop + "<form>{% csrf_token %}</form>{% endfor %}",
        ),
        (
            "Empty loop ({% empty %})",
            (
                "{% for app in empty_apps %}{{ app.candidate.name }}{% empty %}NONE"
                "{% endfor %}"
            ),
        ),
        (
            "{% spaceless %}",
            loop + "{% spaceless %}<tr>  <td>  {{ app.candidate.name }}  </td>  </tr>"
            "{% endspaceless %}{% endfor %}",
        ),
        (
            "Custom Python filter",
            "{% load bench %}"
            + loop
            + "{{ app.candidate.name|bench_noop }}{% endfor %}",
        ),
        (
            "Custom simple_tag",
            "{% load bench %}" + loop + "{% bench_simple_tag app.candidate.name %}"
            "{% endfor %}",
        ),
        (
            "Custom @register.tag",
            "{% load bench %}" + loop + "{% bench_raw_tag app.candidate.name %}"
            "{% endfor %}",
        ),
        (
            "{% regroup %}",
            (
                "{% regroup applications by status as grouped %}{% for g in grouped %}"
                "{{ g.grouper }}({% for a in g.list %}{{ a.id }},{% endfor %}){% endfor %}"
            ),
        ),
        (
            "Filter with a variable argument",
            loop + "{{ app.posting.title|default:app.candidate.name }}{% endfor %}",
        ),
        ("{% include %} per row", loop + "{% include 'bench_row.html' %}{% endfor %}"),
        ("{% extends %} with three blocks", "{% include 'bench_child.html' %}"),
        (
            "Three-level {% extends %} with block.super",
            "{% include 'bench_grandchild.html' %}",
        ),
    ]
    return [Case(label, source=source, context=apps) for label, source in sources]


class BenchForm(forms.Form):
    name = forms.CharField(max_length=40, help_text="Full name")
    email = forms.EmailField()
    role = forms.ChoiceField(choices=[("a", "Admin"), ("m", "Member"), ("v", "Viewer")])
    notify = forms.BooleanField(required=False)


def _aware(day):
    return timezone.make_aware(
        datetime.datetime(2024, 1, 1) + datetime.timedelta(days=day)
    )


def build_users(count):
    return [
        User(
            id=i + 1,
            username=f"user{i}",
            first_name=f"First{i}",
            last_name=f"Last{i}",
            email=f"user{i}@example.com",
            is_staff=i % 5 == 0,
            date_joined=_aware(i),
            last_login=_aware(i + 30) if i % 3 else None,
        )
        for i in range(count)
    ]


def build_permissions(count):
    models = [
        ContentType(app_label="auth", model=name)
        for name in ("user", "group", "permission")
    ]
    return [
        Permission(
            id=i + 1,
            name=f"Can do thing {i}",
            codename=f"thing_{i}",
            content_type=models[i % len(models)],
        )
        for i in range(count)
    ]


def seed_database(count):
    User.objects.bulk_create(build_users(count))


def django_cases(items):
    users = build_users(items)
    permissions = build_permissions(items)
    labels = [
        gettext_lazy(word) for word in ("Active", "Archived", "Hired", "Rejected")
    ]
    lazy = [labels[i % len(labels)] for i in range(items)]
    form = BenchForm(data={"name": "", "email": "not-an-email", "role": "m"})
    form.is_valid()
    return [
        Case(
            "Model instances (User fields and methods)",
            source="{% for u in users %}<tr><td>{{ u.get_full_name }}</td><td>{{ u.email }}</td>"
            '<td>{{ u.date_joined|date:"M d, Y" }}</td>'
            "<td>{% if u.is_staff %}staff{% else %}member{% endif %}</td>"
            '<td>{{ u.last_login|default:"never" }}</td></tr>{% endfor %}',
            context=lambda: {"users": users},
        ),
        Case(
            "Foreign keys (Permission to ContentType)",
            source="{% for p in permissions %}{{ p.content_type.app_label }}."
            "{{ p.codename }}: {{ p }}<br>{% endfor %}",
            context=lambda: {"permissions": permissions},
        ),
        Case(
            "QuerySet queried during render",
            source="{% for u in users %}{{ u.username }} {% endfor %}({{ users|length }})",
            context=lambda: {"users": User.objects.order_by("id")[:items]},
            fresh_context=True,
        ),
        Case(
            "Lazy translation strings",
            source="{% for label in labels %}{{ label }}|{{ label|upper }}"
            "{% if label %}!{% endif %}{% endfor %}",
            context=lambda: {"labels": lazy},
        ),
        Case(
            "Form fields with errors",
            source="{% for field in form %}<div>{{ field.label_tag }}{{ field }}"
            "{{ field.errors }}{{ field.help_text }}</div>{% endfor %}"
            "{{ form.non_field_errors }}",
            context=lambda: {"form": form},
        ),
    ]


def _request(path, user):
    request = RequestFactory().get(path)
    request.user = user
    request.session = SessionStore()
    request._messages = FallbackStorage(request)
    return request


def _superuser():
    return User(
        id=1, username="admin", is_staff=True, is_superuser=True, is_active=True
    )


def page_cases():
    login_request = _request("/admin/login/", AnonymousUser())
    index_request = _request("/admin/", _superuser())

    def login_context():
        return {
            **admin.site.each_context(login_request),
            "title": "Log in",
            "app_path": login_request.get_full_path(),
            "form": AuthenticationForm(login_request),
            "username": "",
            "next": "/admin/",
        }

    def index_context():
        return {
            **admin.site.each_context(index_request),
            "title": "Site administration",
            "subtitle": None,
            "app_list": admin.site.get_app_list(index_request),
        }

    return [
        Case(
            "Django admin login page",
            template_name="admin/login.html",
            context=login_context,
            request=lambda: login_request,
        ),
        Case(
            "Django admin index page",
            template_name="admin/index.html",
            context=index_context,
            request=lambda: index_request,
        ),
    ]


def cotton_page_case(items):
    return Case(
        "django-cotton page (layout, cards, badges)",
        template_name="bench_cotton_page.html",
        context=lambda: {"applications": build_applications(items)},
    )


def _compile_template(rows):
    row = (
        '<tr class="row {% if app.is_archived %}archived{% else %}active{% endif %}">'
        "<td>{{ app.candidate.name }}</td>"
        '<td>{{ app.posting.title|default:"-" }}</td>'
        '<td>{{ app.posting.company.name|default:"-" }}</td>'
        "<td>{{ app.stage.name }}</td>"
        '<td>{{ app.created_at|date:"M d, Y" }}</td>'
        '<td class="status-{{ app.status }}">{{ app.status|title }}</td>'
        "</tr>"
    )
    return "<table><tbody>" + row * rows + "</tbody></table>"


COMPILE_CASES = [
    ("Small (10 rows, about 120 nodes)", _compile_template(10)),
    ("Medium (100 rows, about 1,200 nodes)", _compile_template(100)),
    ("Large (500 rows, about 6,000 nodes)", _compile_template(500)),
]

SCALING_ROWS = (1, 10, 100, 1000)
THREAD_COUNTS = (1, 2, 4, 8)

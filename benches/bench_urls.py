from django.contrib import admin
from django.http import HttpResponse
from django.urls import path


def detail(request, pk):
    return HttpResponse("")


urlpatterns = [
    path("apps/<int:pk>/", detail, name="detail"),
    path("admin/", admin.site.urls),
]

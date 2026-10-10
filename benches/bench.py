import argparse
import datetime
import importlib.metadata
import json
import os
import platform
import resource
import subprocess
import sys
import sysconfig
from pathlib import Path

import django
import harness
import report
import setup_env
from django.template import Context
from django_template_oxide._rust import get_prof_stats, reset_prof_stats
from rich.progress import Progress, SpinnerColumn, TextColumn, TimeElapsedColumn

SECTIONS = (
    "render",
    "django",
    "pages",
    "compile",
    "loading",
    "scaling",
    "context",
    "threads",
    "memory",
)
MB = 1024 * 1024


def _run_command(command):
    try:
        return subprocess.run(
            command, capture_output=True, text=True, check=True
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return ""


def _machine():
    if sys.platform == "darwin":
        name = ""
        for line in _run_command(
            ["system_profiler", "SPHardwareDataType"]
        ).splitlines():
            if "Model Name" in line:
                name = line.split(":", 1)[1].strip()
        model = _run_command(["sysctl", "-n", "hw.model"])
        chip = _run_command(["sysctl", "-n", "machdep.cpu.brand_string"])
        memory = int(_run_command(["sysctl", "-n", "hw.memsize"]) or 0) // (1024**3)
        system = f"macOS {platform.mac_ver()[0]}"
        return f"{name} ({model})".strip(), chip, memory, system
    chip = ""
    memory = 0
    if os.path.exists("/proc/cpuinfo"):
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                chip = line.split(":", 1)[1].strip()
                break
    if os.path.exists("/proc/meminfo"):
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal"):
                memory = int(line.split()[1]) // (1024**2)
    return platform.node(), chip or platform.processor(), memory, platform.platform()


def _rusty_version():
    version = importlib.metadata.version("django-rusty-templates")
    try:
        direct = json.loads(
            importlib.metadata.distribution("django-rusty-templates").read_text(
                "direct_url.json"
            )
            or "{}"
        )
        commit = direct.get("vcs_info", {}).get("commit_id", "")[:7]
    except (OSError, ValueError):
        commit = ""
    return f"{version} ({commit})" if commit else version


def _oxide_version():
    version = importlib.metadata.version("django-template-oxide")
    commit = _run_command(
        ["git", "-C", str(setup_env.BENCH_DIR), "rev-parse", "--short", "HEAD"]
    )
    dirty = _run_command(
        ["git", "-C", str(setup_env.BENCH_DIR), "status", "--porcelain"]
    )
    if not commit:
        return version
    return f"{version} ({commit}{', uncommitted changes' if dirty else ''})"


def collect_meta(args):
    machine, chip, memory, system = _machine()
    free_threaded = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
    gil = getattr(sys, "_is_gil_enabled", lambda: True)()
    python = f"CPython {platform.python_version()}" + (
        f", free-threaded (GIL {'on' if gil else 'off'})" if free_threaded else ""
    )
    oxide = _oxide_version()
    cores = os.cpu_count()
    describe = [
        ("Machine", machine),
        ("Chip", f"{chip}, {cores} cores" if chip else f"{cores} cores"),
        ("Memory", f"{memory} GB" if memory else "unknown"),
        ("OS", system),
        ("Python", python),
        ("Django", django.get_version()),
        ("oxide", oxide),
        ("rusty", _rusty_version()),
        (
            "Method",
            (
                f"Each time is the median of {args.repeats} samples of at least "
                f"{args.target_ms:g} ms, alternating engines. A yellow ± marks a "
                "result whose samples varied by 5% or more."
            ),
        ),
        (
            "Reading",
            (
                "The vs columns show how many times faster oxide is. Red means oxide "
                "is slower, and same means the difference is within measurement noise."
            ),
        ),
    ]
    return {
        "timestamp": datetime.datetime.now().isoformat(timespec="seconds"),
        "machine": machine,
        "python": python,
        "oxide": oxide,
        "settings": {
            "items": args.items,
            "repeats": args.repeats,
            "target_ms": args.target_ms,
            "sections": args.sections,
        },
        "describe": describe,
    }


def _timing_section(key, title, caption, rows):
    return {
        "key": key,
        "kind": "timing",
        "title": title,
        "caption": caption,
        "engines": list(setup_env.ENGINE_NAMES),
        "rows": rows,
    }


def wanted(args, label):
    return not args.only or any(text.lower() in label.lower() for text in args.only)


def run_cases(cases_list, engines, args, progress):
    rows = []
    for case in cases_list:
        if not wanted(args, case.label):
            continue
        progress.update(progress.task_ids[0], description=case.label)
        callables = harness.case_renderers(engines, case)
        rows.append(
            {
                "label": case.label,
                "results": harness.measure(
                    callables, args.repeats, args.target_ms / 1000
                ),
            }
        )
        if args.profile and callable(callables.get("oxide")):
            rows[-1]["profile"] = profile_zones(callables["oxide"])
    return rows


def run_built(label, engines, build, args, progress, check_output=True):
    if not wanted(args, label):
        return None
    progress.update(progress.task_ids[0], description=label)
    callables = harness.checked_callables(engines, build, check_output)
    return {
        "label": label,
        "results": harness.measure(callables, args.repeats, args.target_ms / 1000),
    }


def profile_zones(render, renders=500):
    render()
    reset_prof_stats()
    for _ in range(renders):
        render()
    stats = dict(get_prof_stats())
    return [
        {
            "zone": zone,
            "calls_per_render": values["count"] / renders,
            "ns_per_render": values["total_us"] * 1000 / renders,
        }
        for zone, values in sorted(
            stats.items(), key=lambda item: -item[1]["total_us"]
        )[:12]
    ]


def section_render(engines, args, progress, cases):
    rows = run_cases(cases.render_cases(args.items), engines, args, progress)
    return _timing_section(
        "render",
        "Template features",
        f"Time to render one template over {args.items} rows of plain Python objects.",
        rows,
    )


def section_django(engines, args, progress, cases):
    rows = run_cases(cases.django_cases(args.items), engines, args, progress)
    return _timing_section(
        "django",
        "Django objects",
        f"Time to render {args.items} model instances, foreign keys, a QuerySet, lazy "
        "translations, and a form. The QuerySet row includes its database query.",
        rows,
    )


def section_pages(engines, args, progress, cases):
    rows = run_cases(cases.page_cases(), engines, args, progress)
    if wanted(args, "django-cotton page"):
        progress.update(
            progress.task_ids[0], description="django-cotton page (separate process)"
        )
        rows.append(_worker_result(["cotton"], args))
    return _timing_section(
        "pages",
        "Whole pages",
        "Time to render real pages through get_template and a request, including "
        "inheritance, includes, and context processors.",
        rows,
    )


def section_compile(engines, args, progress, cases):
    rows = [
        row
        for label, src in cases.COMPILE_CASES
        if (
            row := run_built(
                label,
                engines,
                lambda engine, src=src: lambda: engine.from_string(src),
                args,
                progress,
                check_output=False,
            )
        )
    ]
    return _timing_section(
        "compile",
        "Compiling templates",
        "Time to compile a template from source with from_string, with no caching.",
        rows,
    )


def section_loading(engines, args, progress, cases):
    uncached = setup_env.build_engines(cached=False)
    context = {"applications": cases.build_applications(args.items)}

    def build(engine):
        return lambda: engine.get_template("bench_grandchild.html").render(context)

    rows = [
        row
        for row in (
            run_built("Cached loader (production)", engines, build, args, progress),
            run_built(
                "No cache (compiles every time)", uncached, build, args, progress
            ),
        )
        if row
    ]
    return _timing_section(
        "loading",
        "Loading templates",
        "Time for get_template plus render of a three-level template, the way views "
        "load templates.",
        rows,
    )


def section_scaling(engines, args, progress, cases):
    rows = []
    for count in cases.SCALING_ROWS:
        context = {"applications": cases.build_applications(count)}
        label = f"{count:,} row" + ("" if count == 1 else "s")

        def build(engine, context=context):
            template = engine.from_string(cases.FULL_TEMPLATE)
            return lambda: template.render(context)

        row = run_built(label, engines, build, args, progress)
        if row:
            rows.append(row)
    return _timing_section(
        "scaling",
        "Scaling with data size",
        "Time to render the full table template as the number of rows grows.",
        rows,
    )


def section_context(engines, args, progress, cases):
    apps = cases.build_applications(args.items)
    narrow = {"applications": apps}
    wide = {**{f"k{i}": i for i in range(200)}, "applications": apps}

    def dict_build(context):
        def build(engine):
            template = engine.from_string(cases.FULL_TEMPLATE)
            return lambda: template.render(context)

        return build

    def context_build(engine):
        template = engine.from_string(cases.FULL_TEMPLATE).template
        django_context = Context(narrow)
        return lambda: template.render(django_context)

    rows = [
        row
        for row in (
            run_built("Plain dict", engines, dict_build(narrow), args, progress),
            run_built(
                "Django Context object (low-level API)",
                engines,
                context_build,
                args,
                progress,
            ),
            run_built(
                "Dict with 200 extra keys", engines, dict_build(wide), args, progress
            ),
        )
        if row
    ]
    return _timing_section(
        "context",
        "Passing the context",
        f"Time to render the full table template ({args.items} rows) with different "
        "kinds of context.",
        rows,
    )


def section_threads(engines, args, progress, cases):
    context = {"applications": cases.build_applications(args.items)}

    def build(engine):
        template = engine.from_string(cases.FULL_TEMPLATE)
        return lambda: template.render(context)

    callables = harness.checked_callables(engines, build)
    duration = 0.2 if args.quick else 0.5
    rows = []
    for threads in cases.THREAD_COUNTS:
        progress.update(progress.task_ids[0], description=f"{threads} thread(s)")
        results = {}
        for name, fn in callables.items():
            if not callable(fn):
                results[name] = fn
                continue
            try:
                results[name] = harness.throughput(fn, threads, duration)
            except Exception as error:
                results[name] = harness.failure_from(error)
        rows.append({"label": str(threads), "results": results})
    gil = getattr(sys, "_is_gil_enabled", lambda: True)()
    note = (
        "With the GIL on, threads take turns, so throughput should stay flat."
        if gil
        else "With the GIL off, throughput should grow with the thread count."
    )
    return {
        "key": "threads",
        "kind": "throughput",
        "title": "Rendering from several threads",
        "caption": f"Renders per second of the full table template ({args.items} rows), "
        f"all threads sharing one compiled template. {note}",
        "engines": list(setup_env.ENGINE_NAMES),
        "rows": rows,
    }


def section_memory(engines, args, progress, cases):
    rows = {}
    for name in setup_env.ENGINE_NAMES:
        progress.update(
            progress.task_ids[0], description=f"memory: {name} (separate process)"
        )
        rows[name] = _worker_json(["memory", "--engine", name], args)
    return {
        "key": "memory",
        "kind": "memory",
        "title": "Memory",
        "caption": "Extra peak memory (resident set size) of a fresh process for each engine, "
        "including memory allocated in Rust.",
        "engines": list(setup_env.ENGINE_NAMES),
        "rows": rows,
    }


def _worker_command(extra, args):
    return [
        sys.executable,
        str(Path(__file__).resolve()),
        "_worker",
        *extra,
        "--items",
        str(args.items),
        "--repeats",
        str(args.repeats),
        "--target-ms",
        str(args.target_ms),
    ]


def _worker_json(extra, args):
    completed = subprocess.run(
        _worker_command(extra, args), check=False, capture_output=True, text=True
    )
    if completed.returncode != 0:
        return {"error": "worker failed", "detail": completed.stderr[-300:]}
    return json.loads(completed.stdout.strip().splitlines()[-1])


def _worker_result(extra, args):
    data = _worker_json(extra, args)
    if "error" in data:
        return {
            "label": "django-cotton page",
            "results": {
                name: harness.Failure(data["error"], data.get("detail", ""))
                for name in setup_env.ENGINE_NAMES
            },
        }
    return {
        "label": data["label"],
        "results": {
            name: harness.from_json(result) for name, result in data["results"].items()
        },
    }


def _max_rss():
    value = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return value if sys.platform == "darwin" else value * 1024


def worker(args):
    if args.task == "cotton":
        setup_env.configure(cotton=True)
        import cases

        engines = setup_env.build_engines(cotton=True)
        case = cases.cotton_page_case(args.items)
        results = harness.measure(
            harness.case_renderers(engines, case), args.repeats, args.target_ms / 1000
        )
        print(
            json.dumps(
                {
                    "label": case.label,
                    "results": {k: v.to_json() for k, v in results.items()},
                }
            )
        )
        return 0
    setup_env.configure()
    import cases

    engine = setup_env.build_engines()[args.engine]
    if isinstance(engine, Exception):
        print(json.dumps({"error": "can't start", "detail": str(engine)[:300]}))
        return 0
    try:
        small = engine.from_string(cases.FULL_TEMPLATE)
        small.render({"applications": cases.build_applications(10)})
        rows = {"applications": cases.build_applications(1000)}
        start = _max_rss()
        compiled = engine.from_string(cases.COMPILE_CASES[-1][1])
        after_compile = _max_rss()
        template = engine.from_string(cases.FULL_TEMPLATE)
        for _ in range(20):
            template.render(rows)
        after_render = _max_rss()
    except Exception as error:
        failure = harness.failure_from(error)
        print(json.dumps({"error": failure.reason, "detail": failure.detail}))
        return 0
    del compiled
    print(
        json.dumps(
            {
                "compile_mb": (after_compile - start) / MB,
                "render_mb": (after_render - after_compile) / MB,
            }
        )
    )
    return 0


SECTION_RUNNERS = {
    "render": section_render,
    "django": section_django,
    "pages": section_pages,
    "compile": section_compile,
    "loading": section_loading,
    "scaling": section_scaling,
    "context": section_context,
    "threads": section_threads,
    "memory": section_memory,
}


def _profiler_available(engines):
    oxide = engines["oxide"]
    if isinstance(oxide, Exception):
        return False
    reset_prof_stats()
    oxide.from_string("{{ x }}").render({"x": 1})
    return bool(get_prof_stats())


def run(args):
    setup_env.configure()
    import cases

    cases.seed_database(args.items)
    console = report.make_console(args.no_color)
    engines = setup_env.build_engines()
    if args.profile and not _profiler_available(engines):
        console.print(
            "--profile needs oxide built with the profiler:\n"
            "  VIRTUAL_ENV=.venv uvx maturin develop --release --features prof",
            style="red",
        )
        return 2
    meta = collect_meta(args)
    console.print(report.header(meta))
    sections = []
    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        TimeElapsedColumn(),
        console=console,
        transient=True,
    ) as progress:
        progress.add_task("starting")
        for key in args.sections:
            section = SECTION_RUNNERS[key](engines, args, progress, cases)
            sections.append(section)
            report.print_section(progress.console, section)
    summary = report.summarize(sections)
    if summary is not None:
        console.print(summary)
    if args.json:
        payload = {
            "meta": meta,
            "sections": [report.section_to_json(s) for s in sections],
        }
        Path(args.json).write_text(json.dumps(payload, indent=2))
        console.print(f"Saved results to {args.json}", style="dim")
    return 0


def compare(args):
    console = report.make_console(args.no_color)
    old = json.loads(Path(args.before).read_text())
    new = json.loads(Path(args.after).read_text())
    regressions = report.compare(console, old, new, harness.from_json)
    return 1 if regressions else 0


def parse_args(argv):
    parser = argparse.ArgumentParser(
        prog="bench.py",
        description="Compare oxide with django-rusty-templates and stock Django.",
    )
    commands = parser.add_subparsers(dest="command", metavar="{run,compare}")

    run_parser = commands.add_parser("run", help="run the benchmarks (default)")
    run_parser.add_argument(
        "--sections",
        default=",".join(SECTIONS),
        help=f"comma-separated sections to run (default: all of {', '.join(SECTIONS)})",
    )
    run_parser.add_argument(
        "--items", type=int, default=50, help="rows of data (default 50)"
    )
    run_parser.add_argument(
        "--repeats", type=int, help="samples per result (default 9, quick 5)"
    )
    run_parser.add_argument(
        "--target-ms",
        type=float,
        help="minimum sample length in ms (default 10, quick 3)",
    )
    run_parser.add_argument(
        "--quick", action="store_true", help="fewer and shorter samples"
    )
    run_parser.add_argument(
        "--only",
        action="append",
        default=[],
        metavar="TEXT",
        help="only run workloads whose name contains TEXT (repeatable, case-insensitive)",
    )
    run_parser.add_argument(
        "--profile",
        action="store_true",
        help="show where oxide spends time in each workload (needs a build with --features prof)",
    )
    run_parser.add_argument("--json", help="also save results to this JSON file")
    run_parser.add_argument("--no-color", action="store_true")

    compare_parser = commands.add_parser(
        "compare", help="compare oxide between two saved runs"
    )
    compare_parser.add_argument("before")
    compare_parser.add_argument("after")
    compare_parser.add_argument("--no-color", action="store_true")

    worker_parser = commands.add_parser("_worker")
    worker_parser.add_argument("task", choices=["cotton", "memory"])
    worker_parser.add_argument("--engine", choices=setup_env.ENGINE_NAMES)
    worker_parser.add_argument("--items", type=int, default=50)
    worker_parser.add_argument("--repeats", type=int, default=9)
    worker_parser.add_argument("--target-ms", type=float, default=10)

    if not argv or argv[0] not in ("run", "compare", "_worker", "-h", "--help"):
        argv = ["run", *argv]
    args = parser.parse_args(argv)
    if args.command == "run":
        args.repeats = args.repeats or (5 if args.quick else 9)
        args.target_ms = args.target_ms or (3.0 if args.quick else 10.0)
        args.sections = [s.strip() for s in args.sections.split(",") if s.strip()]
        unknown = [s for s in args.sections if s not in SECTIONS]
        if unknown:
            parser.error(f"unknown section(s): {', '.join(unknown)}")
    return args


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.command == "compare":
        return compare(args)
    if args.command == "_worker":
        return worker(args)
    return run(args)


if __name__ == "__main__":
    sys.exit(main())

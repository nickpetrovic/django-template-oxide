import math

from harness import Failure, Measurement, Throughput
from rich import box
from rich.console import Console, Group
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

ENGINE_LABELS = {"oxide": "oxide", "rusty": "rusty", "stock": "stock Django"}


def format_time(ns):
    if ns < 1_000:
        return f"{ns:.0f} ns"
    if ns < 10_000:
        return f"{ns / 1_000:.2f} µs"
    if ns < 1_000_000:
        return f"{ns / 1_000:.1f} µs"
    if ns < 10_000_000:
        return f"{ns / 1_000_000:.2f} ms"
    return f"{ns / 1_000_000:.1f} ms"


def format_rate(value):
    return f"{value:,.0f}/s"


NOISY = 0.05


def _spread(value):
    if value < NOISY:
        return Text("")
    return Text(f" ±{value * 100:.0f}%", style="yellow")


def result_cell(result):
    if isinstance(result, Measurement):
        return Text(format_time(result.median_ns)) + _spread(result.spread)
    if isinstance(result, Throughput):
        return Text(format_rate(result.renders_per_second)) + _spread(result.spread)
    if isinstance(result, Failure):
        style = "bold red" if result.reason == "wrong output" else "dim italic"
        return Text(result.reason, style=style)
    return Text("-", style="dim")


def speedup(oxide, other):
    if not isinstance(oxide, Measurement) or not isinstance(other, Measurement):
        return None
    if oxide.median_ns <= 0:
        return None
    return other.median_ns / oxide.median_ns


def verdict(oxide, other):
    ratio = speedup(oxide, other)
    if ratio is None:
        return Text("")
    noise = max(0.03, oxide.spread + other.spread)
    if abs(ratio - 1) <= noise:
        return Text("same", style="yellow")
    if ratio > 1:
        return Text(_factor(ratio), style="green")
    return Text(f"{_factor(1 / ratio)} slower", style="red")


def _factor(value):
    if value < 1.1:
        return f"{value:.2f}×"
    if value < 10:
        return f"{value:.1f}×"
    return f"{value:.0f}×"


def timing_table(section):
    table = Table(
        title=section["title"],
        caption=section["caption"],
        caption_justify="left",
        title_justify="left",
        title_style="bold",
        box=box.SIMPLE_HEAVY,
        expand=False,
    )
    table.add_column(
        "Workload", style="bold", min_width=20, max_width=42, overflow="fold"
    )
    for name in section["engines"]:
        table.add_column(ENGINE_LABELS[name], justify="right", no_wrap=True)
    others = [name for name in section["engines"] if name != "oxide"]
    for name in others:
        table.add_column(
            f"vs {ENGINE_LABELS[name].split()[0]}", justify="right", no_wrap=True
        )
    for row in section["rows"]:
        results = row["results"]
        cells = [Text(row["label"])]
        cells += [result_cell(results.get(name)) for name in section["engines"]]
        cells += [verdict(results.get("oxide"), results.get(name)) for name in others]
        table.add_row(*cells)
    return table


def throughput_table(section):
    table = Table(
        title=section["title"],
        caption=section["caption"],
        caption_justify="left",
        title_justify="left",
        title_style="bold",
        box=box.SIMPLE_HEAVY,
    )
    table.add_column("Threads", justify="right", style="bold")
    for name in section["engines"]:
        table.add_column(ENGINE_LABELS[name], justify="right", no_wrap=True)
        table.add_column("vs 1 thread", justify="right", style="dim")
    baseline = {}
    for row in section["rows"]:
        cells = [Text(row["label"])]
        for name in section["engines"]:
            result = row["results"].get(name)
            cells.append(result_cell(result))
            if isinstance(result, Throughput):
                base = baseline.setdefault(name, result.renders_per_second)
                cells.append(Text(f"{result.renders_per_second / base:.1f}×"))
            else:
                cells.append(Text(""))
        table.add_row(*cells)
    return table


def memory_table(section):
    table = Table(
        title=section["title"],
        caption=section["caption"],
        caption_justify="left",
        title_justify="left",
        title_style="bold",
        box=box.SIMPLE_HEAVY,
    )
    table.add_column("Engine", style="bold")
    table.add_column("Compile the large template", justify="right")
    table.add_column("Render 1,000 rows", justify="right")
    for name in section["engines"]:
        result = section["rows"].get(name, {})
        if "error" in result:
            failure = Text(result["error"], style="dim italic")
            table.add_row(ENGINE_LABELS[name], failure, failure)
            continue
        table.add_row(
            ENGINE_LABELS[name],
            f"{result['compile_mb']:.1f} MB",
            f"{result['render_mb']:.1f} MB",
        )
    return table


def header(meta):
    grid = Table.grid(padding=(0, 2))
    grid.add_column(style="dim")
    grid.add_column()
    for key, value in meta["describe"]:
        grid.add_row(key, value)
    return Panel(grid, title="Benchmark setup", title_align="left", box=box.ROUNDED)


def _geomean(values):
    return math.exp(sum(math.log(v) for v in values) / len(values)) if values else None


def summarize(sections):
    lines = []
    for other in ("rusty", "stock"):
        ratios = []
        failures = 0
        total = 0
        for section in sections:
            if section["kind"] != "timing" or other not in section["engines"]:
                continue
            for row in section["rows"]:
                oxide = row["results"].get("oxide")
                result = row["results"].get(other)
                if not isinstance(oxide, Measurement):
                    continue
                total += 1
                ratio = speedup(oxide, result)
                if ratio is None:
                    failures += 1
                else:
                    ratios.append(ratio)
        mean = _geomean(ratios)
        if mean is None:
            continue
        label = ENGINE_LABELS[other]
        text = Text("oxide is ")
        text.append(
            f"{mean:.1f}× faster" if mean >= 1 else f"{1 / mean:.1f}× slower",
            style="bold green" if mean >= 1 else "bold red",
        )
        text.append(
            f" than {label} on average (geometric mean of {len(ratios)} workloads both can run)."
        )
        if failures:
            text.append(
                f" {label} could not run {failures} of {total} workloads.", style="dim"
            )
        wins = sum(1 for ratio in ratios if ratio > 1.03)
        losses = sum(1 for ratio in ratios if ratio < 0.97)
        text.append(
            f" Faster on {wins}, slower on {losses}, within 3% on {len(ratios) - wins - losses}.",
            style="dim",
        )
        lines.append(text)
    return (
        Panel(Group(*lines), title="Summary", title_align="left", box=box.ROUNDED)
        if lines
        else None
    )


def profile_table(row):
    table = Table(
        title=f"Where oxide spends time: {row['label']}",
        title_justify="left",
        title_style="bold",
        box=box.SIMPLE,
    )
    table.add_column("Zone")
    table.add_column("Calls per render", justify="right")
    table.add_column("Time per render", justify="right")
    for zone in row["profile"]:
        table.add_row(
            zone["zone"],
            f"{zone['calls_per_render']:,.0f}",
            format_time(zone["ns_per_render"]),
        )
    return table


def print_section(console, section):
    renderers = {
        "timing": timing_table,
        "throughput": throughput_table,
        "memory": memory_table,
    }
    if not section["rows"]:
        return
    console.print(renderers[section["kind"]](section))
    if section["kind"] == "timing":
        for row in section["rows"]:
            if row.get("profile"):
                console.print(profile_table(row))


def section_to_json(section):
    data = {key: value for key, value in section.items() if key != "rows"}
    if section["kind"] == "memory":
        data["rows"] = section["rows"]
        return data
    data["rows"] = [
        {
            "label": row["label"],
            "results": {
                name: result.to_json() for name, result in row["results"].items()
            },
            **({"profile": row["profile"]} if row.get("profile") else {}),
        }
        for row in section["rows"]
    ]
    return data


def compare(console, old, new, from_json):
    console.print(
        Panel(
            Group(
                Text.assemble(("Before  ", "dim"), _describe_short(old["meta"])),
                Text.assemble(("After   ", "dim"), _describe_short(new["meta"])),
            ),
            title="Comparing oxide between two runs",
            title_align="left",
            box=box.ROUNDED,
        )
    )
    if old["meta"].get("machine") != new["meta"].get("machine"):
        console.print(
            Text(
                "The two runs are from different machines, so differences may not be caused by code changes.",
                style="yellow",
            )
        )
    old_sections = {section["key"]: section for section in old["sections"]}
    regressions = 0
    for section in new["sections"]:
        previous = old_sections.get(section["key"])
        if previous is None or section["kind"] != "timing":
            continue
        before = {row["label"]: row["results"].get("oxide") for row in previous["rows"]}
        table = Table(
            title=section["title"],
            title_justify="left",
            title_style="bold",
            box=box.SIMPLE_HEAVY,
        )
        table.add_column("Workload", style="bold", no_wrap=True)
        table.add_column("Before", justify="right")
        table.add_column("After", justify="right")
        table.add_column("Change", justify="right")
        for row in section["rows"]:
            after = (
                from_json(row["results"]["oxide"])
                if "oxide" in row["results"]
                else None
            )
            prior = before.get(row["label"])
            prior = from_json(prior) if prior is not None else None
            change = verdict(after, prior)
            if (
                isinstance(after, Measurement)
                and isinstance(prior, Measurement)
                and change.plain.endswith("slower")
            ):
                regressions += 1
            table.add_row(row["label"], result_cell(prior), result_cell(after), change)
        console.print(table)
    style = "bold red" if regressions else "bold green"
    console.print(
        Text(
            f"{regressions} workload(s) got slower beyond measurement noise.",
            style=style,
        )
    )
    return regressions


def _describe_short(meta):
    return f"{meta.get('oxide', '?')} on {meta.get('python', '?')}, {meta.get('machine', '?')} ({meta.get('timestamp', '?')})"


def make_console(no_color=False):
    return Console(no_color=no_color, highlight=False)

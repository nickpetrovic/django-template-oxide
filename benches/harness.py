import gc
import re
import statistics
import threading
import time
from dataclasses import dataclass

_CSRF_VALUE = re.compile(r'(name="csrfmiddlewaretoken" value=")[^"]+(")')
_ERROR_REASONS = {
    "NotImplementedError": "not supported",
    "TemplateDoesNotExist": "can't load template",
    "TemplateSyntaxError": "syntax error",
    "InvalidTemplateLibrary": "can't load library",
    "AttributeError": "not supported",
}


@dataclass
class Measurement:
    median_ns: float
    spread: float
    samples_ns: list[float]
    loops: int

    def to_json(self):
        return {
            "median_ns": self.median_ns,
            "spread": self.spread,
            "samples_ns": self.samples_ns,
            "loops": self.loops,
        }


@dataclass
class Throughput:
    renders_per_second: float
    spread: float

    def to_json(self):
        return {"renders_per_second": self.renders_per_second, "spread": self.spread}


@dataclass
class Failure:
    reason: str
    detail: str = ""

    def to_json(self):
        return {"error": self.reason, "detail": self.detail}


def from_json(data):
    if "error" in data:
        return Failure(data["error"], data.get("detail", ""))
    if "renders_per_second" in data:
        return Throughput(data["renders_per_second"], data["spread"])
    return Measurement(
        data["median_ns"], data["spread"], data["samples_ns"], data["loops"]
    )


def failure_from(error):
    name = type(error).__name__
    return Failure(_ERROR_REASONS.get(name, name), f"{name}: {error}"[:300])


def normalize(output):
    return _CSRF_VALUE.sub(r"\1-\2", str(output))


def _bind(template, case):
    request = case.request() if case.request else None
    if case.fresh_context:
        factory = case.context
        if request is None:
            return lambda: template.render(factory())
        return lambda: template.render(factory(), request)
    context = case.context()
    if request is None:
        return lambda: template.render(context)
    return lambda: template.render(context, request)


def case_renderers(engines, case):
    def build(engine):
        if case.source is not None:
            template = engine.from_string(case.source)
        else:
            template = engine.get_template(case.template_name)
        return _bind(template, case)

    return checked_callables(engines, build)


def checked_callables(engines, build, check_output=True):
    callables = {}
    reference = None
    stock = engines.get("stock")
    if check_output and not isinstance(stock, Exception):
        try:
            reference = normalize(build(stock)())
        except Exception:
            reference = None
    for name, engine in engines.items():
        if isinstance(engine, Exception):
            callables[name] = Failure("can't start", str(engine)[:300])
            continue
        try:
            fn = build(engine)
            output = fn()
        except Exception as error:
            callables[name] = failure_from(error)
            continue
        if check_output and reference is not None and normalize(output) != reference:
            callables[name] = Failure("wrong output")
            continue
        callables[name] = fn
    return callables


def _run(fn, loops):
    gc.collect()
    start = time.perf_counter()
    for _ in range(loops):
        fn()
    return time.perf_counter() - start


def _calibrate(fn, target):
    loops = 1
    while True:
        elapsed = _run(fn, loops)
        if elapsed >= target or loops >= 1_000_000:
            return loops
        if elapsed <= 0:
            loops *= 10
        else:
            loops = max(loops * 2, int(loops * target / elapsed * 1.1))


def measure(callables, repeats, target_seconds):
    runnable = {name: fn for name, fn in callables.items() if callable(fn)}
    results = {name: value for name, value in callables.items() if not callable(value)}
    loops = {}
    for name, fn in runnable.items():
        try:
            loops[name] = _calibrate(fn, target_seconds)
            _run(fn, loops[name])
        except Exception as error:
            results[name] = failure_from(error)
    names = [name for name in runnable if name in loops]
    samples = {name: [] for name in names}
    for index in range(repeats):
        shift = index % len(names) if names else 0
        for name in names[shift:] + names[:shift]:
            elapsed = _run(runnable[name], loops[name])
            samples[name].append(elapsed / loops[name] * 1e9)
    for name in names:
        values = samples[name]
        mean = statistics.fmean(values)
        spread = statistics.stdev(values) / mean if len(values) > 1 and mean else 0.0
        results[name] = Measurement(
            statistics.median(values), spread, values, loops[name]
        )
    return results


def _throughput_once(make, threads, duration):
    barrier = threading.Barrier(threads + 1)
    stop = threading.Event()
    counts = [0] * threads
    errors = []

    def worker(slot):
        fn = None
        try:
            fn = make()
            fn()
        except Exception as error:
            errors.append(error)
        barrier.wait()
        if fn is None:
            return
        done = 0
        try:
            while not stop.is_set():
                fn()
                done += 1
        except Exception as error:
            errors.append(error)
        counts[slot] = done

    workers = [threading.Thread(target=worker, args=(slot,)) for slot in range(threads)]
    for thread in workers:
        thread.start()
    barrier.wait()
    start = time.perf_counter()
    time.sleep(duration)
    stop.set()
    for thread in workers:
        thread.join()
    elapsed = time.perf_counter() - start
    if errors:
        raise errors[0]
    return sum(counts) / elapsed


def throughput(make, threads, duration, repeats=3):
    values = [_throughput_once(make, threads, duration) for _ in range(repeats)]
    mean = statistics.fmean(values)
    spread = statistics.stdev(values) / mean if len(values) > 1 and mean else 0.0
    return Throughput(statistics.median(values), spread)

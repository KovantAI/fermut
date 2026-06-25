def add(a: int, b: int) -> int:
    return a + b


def sub(a: int, b: int) -> int:
    return a - b


def is_positive(x: int) -> bool:
    return x > 0


def in_range(x: int, lo: int, hi: int) -> bool:
    return lo <= x and x <= hi


def absval(x: int) -> int:
    if x < 0:
        return -x
    return x


def double_if_truthy(x: int, flag: bool) -> int:
    if not flag:
        return 0
    total = 0
    total += x
    total += x
    return total


def is_none_or_member(x, items: list[int]) -> bool:
    return x is None or x in items


def trace(fn):
    def wrapper(*args, **kw):
        return fn(*args, **kw)
    return wrapper


@trace
def negate(x: int) -> int:
    return -x


def trim(items: list[int], n: int = 5) -> list[int]:
    return items[:n]


def first_neg(items: list[int]) -> int:
    for x in items:
        if x < 0:
            return x
        continue
    return -1


def squared_lambda() -> int:
    sq = lambda x: x * x
    return sq(3)


def safe_div(a: int, b: int) -> int:
    try:
        return a // b
    except ZeroDivisionError:
        return 0


def sum_all(items: list[int]) -> int:
    total = 0
    for x in items:
        total += x
    return total


def stride_take(items: list[int]) -> list[int]:
    return items[::2]


def greet(name: str) -> str:
    """Return a friendly greeting addressed to *name*."""
    msg = "hello"
    return msg + ", " + name


def auth_prefix() -> bytes:
    return b"Bearer "


def configure(name: str, *, debug: bool = False, retries: int = 3) -> dict:
    return {"name": name, "debug": debug, "retries": retries}


def make_request() -> dict:
    return configure("api", debug=True, retries=5)

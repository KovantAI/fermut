from src.calculator import (
    add,
    sub,
    is_positive,
    in_range,
    absval,
    double_if_truthy,
    is_none_or_member,
    negate,
    trim,
    first_neg,
    squared_lambda,
    safe_div,
    sum_all,
)


def test_add():
    assert add(2, 3) == 5
    assert add(-1, 1) == 0


def test_sub():
    assert sub(5, 3) == 2
    assert sub(0, 0) == 0


def test_is_positive():
    assert is_positive(1) is True
    assert is_positive(0) is False
    assert is_positive(-1) is False


def test_in_range():
    # Intentionally weak: doesn't probe the boundary, so <= vs < mutants will survive.
    assert in_range(5, 1, 10) is True
    assert in_range(0, 1, 10) is False


def test_absval():
    assert absval(-3) == 3
    assert absval(3) == 3
    assert absval(0) == 0


def test_double_if_truthy():
    assert double_if_truthy(3, True) == 6
    assert double_if_truthy(3, False) == 0
    assert double_if_truthy(0, True) == 0


def test_is_none_or_member():
    assert is_none_or_member(None, [1, 2, 3]) is True
    assert is_none_or_member(2, [1, 2, 3]) is True
    assert is_none_or_member(5, [1, 2, 3]) is False


def test_negate():
    assert negate(5) == -5
    assert negate(-7) == 7


def test_trim():
    assert trim([1, 2, 3, 4, 5, 6], 3) == [1, 2, 3]
    assert trim([1, 2, 3]) == [1, 2, 3]
    assert trim([1, 2, 3], 0) == []


def test_first_neg():
    assert first_neg([1, -2, 3]) == -2
    assert first_neg([1, 2, 3]) == -1


def test_squared_lambda():
    assert squared_lambda() == 9


def test_safe_div():
    assert safe_div(10, 2) == 5
    assert safe_div(1, 0) == 0


def test_sum_all():
    assert sum_all([1, 2, 3]) == 6
    assert sum_all([]) == 0


def test_stride_take():
    from src.calculator import stride_take

    assert stride_take([1, 2, 3, 4, 5]) == [1, 3, 5]
    assert stride_take([]) == []


def test_greet():
    from src.calculator import greet

    assert greet("world") == "hello, world"


def test_add_commutes():
    # A SECOND test that also exercises `add`, so `add`'s mutated line is
    # covered by two tests. Smart test ordering (`kill-order.json`) only
    # captures/reorders when a mutant is selected by >1 test — with the strict
    # one-test-per-function mapping above there would be nothing to reorder, so
    # this keeps the smart-order e2e meaningful.
    assert add(3, 4) == add(4, 3)

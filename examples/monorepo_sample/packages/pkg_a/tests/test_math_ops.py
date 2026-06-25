from pkg_a.math_ops import add, in_range


def test_add():
    assert add(2, 3) == 5


def test_in_range():
    assert in_range(5, 1, 10) is True
    assert in_range(0, 1, 10) is False

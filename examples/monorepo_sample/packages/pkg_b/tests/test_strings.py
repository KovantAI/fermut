from pkg_b.strings import double, is_small


def test_double():
    assert double(3) == 6


def test_is_small():
    assert is_small(5) is True
    assert is_small(11) is False

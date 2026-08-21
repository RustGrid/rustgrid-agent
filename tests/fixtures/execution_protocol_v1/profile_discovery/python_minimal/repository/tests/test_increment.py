from profile_fixture import increment


def test_increment() -> None:
    assert increment(1) == 2


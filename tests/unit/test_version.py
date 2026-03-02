"""Tests for core/skills/version.py — semantic version parsing and constraint matching."""

import pytest

from core.skills.version import Version, VersionConstraint, parse_version


class TestParseVersion:
    def test_full(self):
        assert parse_version("1.2.3") == Version(1, 2, 3)

    def test_two_parts(self):
        assert parse_version("1.2") == Version(1, 2, 0)

    def test_one_part(self):
        assert parse_version("3") == Version(3, 0, 0)

    def test_whitespace(self):
        assert parse_version("  1.0.0  ") == Version(1, 0, 0)

    def test_invalid(self):
        with pytest.raises(ValueError, match="Invalid version"):
            parse_version("abc")

    def test_empty(self):
        with pytest.raises(ValueError):
            parse_version("")

    def test_negative(self):
        with pytest.raises(ValueError):
            parse_version("-1.0.0")


class TestVersionOrdering:
    def test_equal(self):
        assert Version(1, 0, 0) == Version(1, 0, 0)

    def test_less_major(self):
        assert Version(1, 0, 0) < Version(2, 0, 0)

    def test_less_minor(self):
        assert Version(1, 1, 0) < Version(1, 2, 0)

    def test_less_patch(self):
        assert Version(1, 0, 1) < Version(1, 0, 2)

    def test_greater(self):
        assert Version(2, 0, 0) > Version(1, 9, 9)

    def test_sorting(self):
        versions = [Version(2, 0, 0), Version(1, 0, 0), Version(1, 5, 0)]
        assert sorted(versions) == [Version(1, 0, 0), Version(1, 5, 0), Version(2, 0, 0)]


class TestVersionConstraint:
    def test_gte(self):
        c = VersionConstraint(">=1.0")
        assert c.matches("1.0.0")
        assert c.matches("2.0.0")
        assert not c.matches("0.9.0")

    def test_lt(self):
        c = VersionConstraint("<2.0")
        assert c.matches("1.9.9")
        assert not c.matches("2.0.0")

    def test_eq(self):
        c = VersionConstraint("==1.5.0")
        assert c.matches("1.5.0")
        assert not c.matches("1.5.1")

    def test_neq(self):
        c = VersionConstraint("!=1.5.0")
        assert not c.matches("1.5.0")
        assert c.matches("1.5.1")

    def test_gt(self):
        c = VersionConstraint(">1.0")
        assert c.matches("1.0.1")
        assert not c.matches("1.0.0")

    def test_lte(self):
        c = VersionConstraint("<=1.0")
        assert c.matches("1.0.0")
        assert c.matches("0.9.0")
        assert not c.matches("1.0.1")

    def test_compound(self):
        c = VersionConstraint(">=1.0,<2.0")
        assert c.matches("1.0.0")
        assert c.matches("1.9.9")
        assert not c.matches("0.9.0")
        assert not c.matches("2.0.0")

    def test_tilde_patch(self):
        c = VersionConstraint("~=1.2.3")
        assert c.matches("1.2.3")
        assert c.matches("1.2.9")
        assert not c.matches("1.3.0")
        assert not c.matches("1.2.2")

    def test_tilde_minor(self):
        c = VersionConstraint("~=1.2")
        assert c.matches("1.2.0")
        assert c.matches("1.9.9")
        assert not c.matches("2.0.0")
        assert not c.matches("1.1.9")

    def test_tilde_zero_patch(self):
        """~=1.2.0 (3 segments) means >=1.2.0,<1.3.0 — NOT <2.0.0."""
        c = VersionConstraint("~=1.2.0")
        assert c.matches("1.2.0")
        assert c.matches("1.2.9")
        assert not c.matches("1.3.0")
        assert not c.matches("2.0.0")

    def test_tilde_one_zero(self):
        """~=1.0.0 (3 segments) means >=1.0.0,<1.1.0."""
        c = VersionConstraint("~=1.0.0")
        assert c.matches("1.0.0")
        assert c.matches("1.0.99")
        assert not c.matches("1.1.0")

    def test_wildcard(self):
        c = VersionConstraint("*")
        assert c.matches("0.0.0")
        assert c.matches("99.99.99")

    def test_matches_version_object(self):
        c = VersionConstraint(">=1.0")
        assert c.matches(Version(1, 5, 0))

    def test_invalid_constraint(self):
        with pytest.raises(ValueError, match="Invalid version constraint"):
            VersionConstraint("not_a_constraint")

    def test_repr(self):
        c = VersionConstraint(">=1.0,<2.0")
        assert ">=1.0,<2.0" in repr(c)

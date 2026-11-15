import unittest

from ledger import engine
from spec.support import load_fixture


class MaxAmountSpec(unittest.TestCase):
    def test_flags_entries_above_the_limit(self):
        findings = [f for f in engine.check(load_fixture("basic")) if f.code == "AMT001"]
        self.assertEqual([f.ref for f in findings], ["R-101", "R-102"])

    def test_limit_comes_from_settings(self):
        findings = engine.check(load_fixture("basic"), {"AMT001": {"max_cents": 10}})
        self.assertEqual(len([f for f in findings if f.code == "AMT001"]), 7)


if __name__ == "__main__":
    unittest.main()

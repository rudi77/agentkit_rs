"""Hidden acceptance spec for the task `weekend_block` — copied in by the verifier."""
import unittest

from ledger import engine
from spec.support import load_fixture

CODE = "WKD002"


class WeekendBlockHiddenSpec(unittest.TestCase):
    def findings(self, settings=None):
        return [f for f in engine.check(load_fixture("basic"), settings) if f.code == CODE]

    def test_rule_is_registered(self):
        self.assertIn(CODE, [rule.code for rule in engine.load_rules()])

    def test_default_setting_is_shipped(self):
        self.assertEqual(engine.load_settings().get(CODE), {"allow_accounts": []})

    def test_default_behaviour(self):
        findings = self.findings()
        self.assertEqual([f.ref for f in findings], ['R-102', 'R-103', 'TMP-7'])
        self.assertTrue(all(f.message == "booked on a weekend" for f in findings))

    def test_setting_is_respected(self):
        settings = dict(engine.load_settings())
        settings.update({'WKD002': {'allow_accounts': ['payroll']}})
        self.assertEqual([f.ref for f in self.findings(settings)], ['R-103', 'TMP-7'])


if __name__ == "__main__":
    unittest.main()

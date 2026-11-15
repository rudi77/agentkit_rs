"""Hidden acceptance spec for the task `currency_whitelist` — copied in by the verifier."""
import unittest

from ledger import engine
from spec.support import load_fixture

CODE = "CUR004"


class CurrencyWhitelistHiddenSpec(unittest.TestCase):
    def findings(self, settings=None):
        return [f for f in engine.check(load_fixture("basic"), settings) if f.code == CODE]

    def test_rule_is_registered(self):
        self.assertIn(CODE, [rule.code for rule in engine.load_rules()])

    def test_default_setting_is_shipped(self):
        self.assertEqual(engine.load_settings().get(CODE), {"allowed": ["EUR", "USD", "CHF"]})

    def test_default_behaviour(self):
        findings = self.findings()
        self.assertEqual([f.ref for f in findings], ['TMP-7'])
        self.assertTrue(all(f.message == "currency not allowed" for f in findings))

    def test_setting_is_respected(self):
        settings = dict(engine.load_settings())
        settings.update({'CUR004': {'allowed': ['EUR']}})
        self.assertEqual([f.ref for f in self.findings(settings)], ['R-102', 'R-103', 'TMP-7'])


if __name__ == "__main__":
    unittest.main()

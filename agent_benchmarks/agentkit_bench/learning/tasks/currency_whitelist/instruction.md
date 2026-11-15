Add a new ledger rule `currency_whitelist` with the code `CUR004`.

It reports every entry whose currency is not in the list under the rule's
setting `allowed`, with the message `currency not allowed`. Ship that setting
with `["EUR", "USD", "CHF"]` as its default.

Follow the project's existing conventions for rules, for how they are
registered and for where their settings live. Add a spec for the new rule and
make sure the project's own checks pass before you finish.

Add a new ledger rule `weekend_block` with the code `WKD002`.

It reports every entry that was booked on a Saturday or a Sunday, with the
message `booked on a weekend`. Accounts listed under the rule's setting
`allow_accounts` are exempt. Ship that setting with an empty list as its
default.

Follow the project's existing conventions for rules, for how they are
registered and for where their settings live. Add a spec for the new rule and
make sure the project's own checks pass before you finish.

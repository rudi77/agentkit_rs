Add a new ledger rule `duplicate_ref` with the code `DUP003`.

It reports every entry whose `ref` has already appeared earlier in the list
(the second and every later occurrence, not the first), with the message
`duplicate reference`. References that start with one of the prefixes listed
under the rule's setting `ignore_prefixes` are never reported. Ship that
setting with an empty list as its default.

Follow the project's existing conventions for rules, for how they are
registered and for where their settings live. Add a spec for the new rule and
make sure the project's own checks pass before you finish.

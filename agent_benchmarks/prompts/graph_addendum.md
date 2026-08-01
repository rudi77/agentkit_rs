# Shared knowledge graph

You have access to a knowledge graph through the `graph_*` tools. It is
**shared across all tasks of this benchmark run**: earlier tasks have recorded
what they learned, and what you record is handed to the tasks after you. You
are one worker in a series, not a one-off.

**Start:** call `graph_search` for the technologies this task involves
(language, test runner, libraries). A previous worker may already have paid the
cost of finding something out.

**Before you finish, you MUST call `graph_remember` at least once.** This is
part of the task, not an optional extra. Record what would have saved *you*
time at the start:

- how the tests are run here, and what the exact command was
- where the test files and the file you must edit live, and how they are named
- an API or signature convention the tests expect that was not obvious
- a dead end that cost you a step, so the next worker skips it

**Reuse the names you were just given.** This is the single most important
rule, and the easiest to get wrong. When `graph_search` returns a claim, it
shows you the exact entity names and the predicate it uses. Write your new
claim with *those* names. Inventing a fresh name for the same thing — "react
exercise tests" when the graph already knows "Exercism Python exercise" —
creates a second, disconnected island instead of adding to what is there. The
graph then grows in size but not in usefulness, and the next worker has to
read two half-answers instead of one whole one.

Concretely:

- Anchor on the **stable** thing, not on today's exercise. The subject of a
  useful claim is `Exercism Python exercise`, not `two-bucket exercise tests`.
  What you learned about running the tests is true for every exercise of this
  kind; write it that way.
- Reuse the **predicate** you saw (`use`, not `run with`, if `use` is what the
  existing claim used).
- Prefer the **general** phrasing. If the graph says `pytest -q
  <exercise>_test.py next to the solution module`, do not replace it with
  `pytest -q react_test.py` — that is a step backwards.
- If an existing claim **already covers** what you learned, do not write a
  narrower copy of it. Add only what is genuinely new, or add nothing.

Write triples with short predicates: `Exercism Python exercise --use→ pytest -q
<exercise>_test.py next to the solution module`.

**Then call `graph_promote` on every claim you want to hand on.** This is not
optional bookkeeping — it is the only thing that makes a claim visible to the
next worker. What you write with `graph_remember` lands in your own working
memory and dies with your session; only a promoted claim becomes durable
knowledge the next task can find. Remember, then promote: two calls, and the
work you did survives.

Two rules on content: record **knowledge, not answers** — no solution code,
no function bodies, no task descriptions; and prefer statements that hold for
the *next* exercise of this kind, not just for this one. If you truly believe
this task taught nothing transferable, record that judgement itself as an
observation and say why.

Solving the task remains your primary job. The graph costs you one search at
the start and one or two writes at the end.

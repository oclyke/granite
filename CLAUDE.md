# CLAUDE.md

## Style

Minimalism is the rule. Every comment, test, and file must earn its place.

### Comments

- Do not comment code that speaks for itself. No restating what a line does.
- Comment only what is non-obvious: a subtle invariant, a tricky algorithm, a
  workaround for external behavior, or a decision whose rationale isn't visible
  from the code.
- No docstrings on self-explanatory functions. No section-banner comments, no
  `// end of ...`, no TODO noise.
- Never add comments narrating a change ("added X", "changed to Y") — that's
  what git history is for.

### Tests

- Write the fewest tests that meaningfully cover the behavior.
- Test real behavior and edge cases that could plausibly break. Do not test
  language features, trivial getters, or that a constructor assigns fields.
- One good test beats five variations of the same case. No table of near-
  duplicate inputs unless each row exercises a distinct path.
- Don't add a test just because a function was added.

### Code

- Prefer the smaller change. Don't introduce abstractions, config knobs, or
  error types until there's a second caller that needs them.
- No defensive scaffolding for conditions that can't occur.
- Match the surrounding code's idiom, naming, and density.

### Output

- Don't write summary files, migration notes, or docs unless asked.
- Keep responses short; skip recaps of code the user can read.

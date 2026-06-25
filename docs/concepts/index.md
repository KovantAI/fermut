# Concepts

The mental model behind fermut. Read these if you want to understand
*why* mutation testing works the way it does, not just *how* to run
the tool.

- **[Mutation testing](mutation-testing.md)** — what it measures,
  vocabulary (operator / mutant / survived / killed), the score,
  the filter chain, worker isolation.
- **[Projects](projects.md)** — what fermut considers a project,
  how it discovers the root, source/tests/state layout.
- **[Configuration](configuration.md)** — file vs CLI vs profile,
  precedence rules, discovery walk.
- **[Caching](caching.md)** — content-hash keying, what's cached,
  when to clear, why this is the iteration-speed lever.
- **[Landscape](landscape.md)** — fermut vs mutmut vs cosmic-ray,
  feature comparison, and the motivation for a new tool.
- **[Glossary](glossary.md)** — one-page lookup for every term the
  docs assume you know.

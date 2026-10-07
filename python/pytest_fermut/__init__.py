"""pytest integration for fermut.

- ``plugin``: the ``pytest --fermut`` front-end (registered as a ``pytest11``
  entry point named ``fermut``).
- ``_fermut_reporter``: the per-run result reporter fermut injects into its own
  per-mutant pytest runs with ``-p _fermut_reporter``.
"""

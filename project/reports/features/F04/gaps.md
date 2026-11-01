# F04 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.

- Predicate pushdown / ColumnPrune RBO rules (output Projection is wrapped; filters still run above TableReader)
- Qualified column resolution across joins
- Coprocessor-side filter

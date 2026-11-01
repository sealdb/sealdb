# F04 gaps

PPD and coprocessor-filter are in the core path: CNF conjuncts attach to
TableReader / IndexLookUp and are evaluated at scan time (`task: cop`).

These stay out of the core-ported door.

- TiKV Coprocessor DAG / CopClient protobuf (local scan-time filter only)
- ColumnPrune as a standalone RBO (output Projection is wrapped)
- Outer-join inner-side predicates converting LEFT to Inner

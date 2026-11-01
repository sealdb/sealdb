# F01 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.

- SQL Mode / overflow / `ERROR 1690` exact messages
- Collation-aware string comparison
- Decimal binary encoding (`ToBin`) beyond integer-shaped values
- Vectorized eval (`*_vec.go`)
- `CASE` / `COALESCE` / `NULLIF`
- Division promoting integers to Decimal the way TiDB does in some SQL modes

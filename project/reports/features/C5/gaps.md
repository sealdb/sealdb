# C5 gaps

C5 ports SET NAMES / sql_mode / collation_connection, ASCII
case-insensitive `*_ci` compares, a Latin/ASCII UCA 4.0.0
weight subset for `utf8mb4_unicode_ci` (WHERE / ORDER BY / `=`),
a GBK / latin1 charset MVB, ONLY_FULL_GROUP_BY, and SET GLOBAL:

- `encoding_gbk` / `encoding_latin1` decode/encode roundtrip (ASCII +
  small Chinese / Latin test subset; full GBK tables stay out)
- `gbk_bin` / `gbk_chinese_ci` string compare (subset sort keys)
- `SET NAMES gbk` / `SET NAMES latin1` → default `*_bin` collations
- Session `sql_mode` ONLY_FULL_GROUP_BY: reject clear SELECT/GROUP BY
  violations (non-aggregated columns not in GROUP BY); turning the mode
  off allows the same query via HashAgg any-value
- `SET GLOBAL` for `sql_mode`, `tidb_enable_auto_analyze`, and
  character_set_* / SET NAMES defaults stored on Domain; new sessions
  inherit; `SELECT @@GLOBAL.*` reads Domain; `SET SESSION` / plain SET
  stay session-local

These stay out:

- Full Unicode weight tables beyond the Latin/ASCII subset; pad/space edge cases outside preprocess trim
- Full GBK code-page tables and invalid charset error paths beyond `?` substitution
- Functional-dependency exceptions for ONLY_FULL_GROUP_BY (PK/unique → other columns)
- Remaining SQL modes beyond ONLY_FULL_GROUP_BY
- Persisted sysvars (disk / mysql.global_variables) beyond Domain memory
- Full GLOBAL sysvar surface beyond the MVB set above
- Running leftover `collation_*` / `new_character_set*` suites

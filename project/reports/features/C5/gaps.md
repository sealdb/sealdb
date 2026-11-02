# C5 gaps

C5 ports SET NAMES / sql_mode / collation_connection, ASCII
case-insensitive `*_ci` compares, a Latin/ASCII UCA 4.0.0
weight subset for `utf8mb4_unicode_ci` (WHERE / ORDER BY / `=`),
and a GBK / latin1 charset MVB:

- `encoding_gbk` / `encoding_latin1` decode/encode roundtrip (ASCII +
  small Chinese / Latin test subset; full GBK tables stay out)
- `gbk_bin` / `gbk_chinese_ci` string compare (subset sort keys)
- `SET NAMES gbk` / `SET NAMES latin1` → default `*_bin` collations

These stay out:

- Full Unicode weight tables beyond the Latin/ASCII subset; pad/space edge cases outside preprocess trim
- Full GBK code-page tables and invalid charset error paths beyond `?` substitution
- ONLY_FULL_GROUP_BY enforcement and remaining SQL modes
- SET GLOBAL and persisted sysvars
- Running leftover `collation_*` / `new_character_set*` suites

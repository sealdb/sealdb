# C5 gaps

C5 ports SET NAMES / sql_mode / collation_connection, ASCII
case-insensitive `*_ci` compares, and a Latin/ASCII UCA 4.0.0
weight subset for `utf8mb4_unicode_ci` (WHERE / ORDER BY / `=`).

These stay out:

- Full Unicode weight tables beyond the Latin/ASCII subset; pad/space edge cases outside preprocess trim
- GBK / latin1 conversion and invalid charset errors
- ONLY_FULL_GROUP_BY enforcement and remaining SQL modes
- SET GLOBAL and persisted sysvars
- Running leftover `collation_*` / `new_character_set*` suites

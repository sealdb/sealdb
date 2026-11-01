# C5 gaps

C5 ports SET NAMES / sql_mode / collation_connection and ASCII
case-insensitive compares for `*_ci` collations. These stay out:

- Unicode weight strings, pad/space, accent-sensitive compare
- GBK / latin1 conversion and invalid charset errors
- ONLY_FULL_GROUP_BY enforcement and remaining SQL modes
- SET GLOBAL and persisted sysvars
- Running leftover `collation_*` / `new_character_set*` suites

# F22 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in `pkg/server/conn.rs` and marked
on `pkg/server/internal/packetio.rs` / `pkg/server/server.rs`.

- Compression / zstd
- Prepared statements (`COM_STMT_*`) and binary protocol
- Auth plugins, `mysql.user` password verify (SkipGrant handshake)
- `COM_FIELD_LIST` / `COM_CHANGE_USER` / `COM_RESET_CONNECTION`
- Multi-result-set (`SERVER_MORE_RESULTS_EXISTS`)
- Connection limits, status HTTP, graceful drain
- Empty first-column vs OK-packet disambiguation for `CLIENT_DEPRECATE_EOF`

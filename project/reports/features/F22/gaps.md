# F22 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in `pkg/server/conn.rs` and marked
on `pkg/server/internal/packetio.rs` / `pkg/server/server.rs`.

- zstd compression algorithm negotiation
- Password hash verify beyond empty/native skip (`caching_sha2_password`)
- `COM_FIELD_LIST` / `COM_CHANGE_USER` / `COM_RESET_CONNECTION`
- Connection limits, status HTTP, graceful drain
- Empty first-column vs OK-packet disambiguation for `CLIENT_DEPRECATE_EOF`
- `COM_STMT_SEND_LONG_DATA` / cursor fetch

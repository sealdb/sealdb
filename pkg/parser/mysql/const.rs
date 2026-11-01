pub const UTF8MB4: &str = "utf8mb4";
pub const DEFAULT_COLLATION: &str = "utf8mb4_bin";
pub const SERVER_VERSION: &str = "8.0.11-SealDB-0.1.0";
pub const DEFAULT_CHARSET_ID: u8 = 45; // utf8mb4_general_ci

pub const OK_HEADER: u8 = 0x00;
pub const ERR_HEADER: u8 = 0xff;
pub const EOF_HEADER: u8 = 0xfe;
pub const NULL_LENENC: u8 = 0xfb;

pub const COM_SLEEP: u8 = 0;
pub const COM_QUIT: u8 = 1;
pub const COM_INIT_DB: u8 = 2;
pub const COM_QUERY: u8 = 3;
pub const COM_FIELD_LIST: u8 = 4;
pub const COM_PING: u8 = 14;
pub const COM_STMT_PREPARE: u8 = 22;
pub const COM_STMT_EXECUTE: u8 = 23;

pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

pub const CLIENT_LONG_PASSWORD: u32 = 1;
pub const CLIENT_FOUND_ROWS: u32 = 1 << 1;
pub const CLIENT_LONG_FLAG: u32 = 1 << 2;
pub const CLIENT_CONNECT_WITH_DB: u32 = 1 << 3;
pub const CLIENT_PROTOCOL_41: u32 = 1 << 9;
pub const CLIENT_TRANSACTIONS: u32 = 1 << 13;
pub const CLIENT_SECURE_CONNECTION: u32 = 1 << 15;
pub const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
pub const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
pub const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;
pub const CLIENT_DEPRECATE_EOF: u32 = 1 << 24;

pub const TYPE_LONG: u8 = 3;
pub const TYPE_DOUBLE: u8 = 5;
pub const TYPE_NULL: u8 = 6;
pub const TYPE_LONGLONG: u8 = 8;
pub const TYPE_DATETIME: u8 = 12;
pub const TYPE_NEWDECIMAL: u8 = 0xf6;
pub const TYPE_VAR_STRING: u8 = 0xfd;

pub const DEFAULT_CAPABILITY: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_DEPRECATE_EOF;

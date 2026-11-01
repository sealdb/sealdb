use crate::errcode;

pub fn name_of(code: i32) -> &'static str {
    match code {
        errcode::ERR_DB_CREATE_EXISTS => "ErrDBCreateExists",
        errcode::ERR_DB_DROP_EXISTS => "ErrDBDropExists",
        errcode::ERR_NO_DB => "ErrNoDB",
        errcode::ERR_BAD_DB => "ErrBadDB",
        errcode::ERR_TABLE_EXISTS => "ErrTableExists",
        errcode::ERR_PARSE_ERROR => "ErrParseError",
        errcode::ERR_UNKNOWN_TABLE => "ErrUnknownTable",
        errcode::ERR_NO_SUCH_TABLE => "ErrNoSuchTable",
        errcode::ERR_UNKNOWN_ERROR => "ErrUnknownError",
        errcode::ERR_WRONG_VALUE_COUNT => "ErrWrongValueCount",
        errcode::ERR_BAD_NULL => "ErrBadNull",
        errcode::ERR_DUP_ENTRY => "ErrDupEntry",
        errcode::ERR_DATA_OUT_OF_RANGE => "ErrDataOutOfRange",
        errcode::ERR_TRUNCATED_WRONG_VALUE => "ErrTruncatedWrongValue",
        errcode::ERR_UNKNOWN_FUNCTION => "ErrUnknownFunction",
        errcode::ERR_WRONG_ARGUMENTS => "ErrWrongArguments",
        errcode::ERR_UNSUPPORTED => "ErrNotSupportedYet",
        errcode::ERR_DIVISION_BY_ZERO => "ErrDivisionByZero",
        errcode::ERR_INVALID_KEY => "ErrInvalidKey",
        errcode::ERR_INVALID_RECORD_KEY => "ErrInvalidRecordKey",
        errcode::ERR_INVALID_INDEX_KEY => "ErrInvalidIndexKey",
        _ => "ErrUnknown",
    }
}

pub fn sqlstate_of(code: i32) -> &'static str {
    match code {
        errcode::ERR_NO_DB => "3D000",
        errcode::ERR_BAD_DB => "42000",
        errcode::ERR_TABLE_EXISTS => "42S01",
        errcode::ERR_PARSE_ERROR => "42000",
        errcode::ERR_NO_SUCH_TABLE => "42S02",
        errcode::ERR_DUP_ENTRY => "23000",
        errcode::ERR_DATA_OUT_OF_RANGE => "22003",
        errcode::ERR_DIVISION_BY_ZERO => "22012",
        _ => "HY000",
    }
}

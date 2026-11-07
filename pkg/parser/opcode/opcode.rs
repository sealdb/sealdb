#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    Plus,
    Minus,
    Mul,
    Div,
    /// MySQL `DIV` — integer division (TiDB `opcode.IntDiv`).
    IntDiv,
    Mod,
    Eq,
    /// MySQL/TiDB null-safe equal `<=>`.
    NullSafeEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogicAnd,
    LogicOr,
    /// MySQL `XOR` — logical exclusive or (three-valued).
    LogicXor,
    /// TiDB `opcode.LeftShift` / `RightShift` (`<<`, `>>`).
    LeftShift,
    RightShift,
    Not,
    /// TiDB `opcode.Not2` — MySQL `!` (same truth table as `NOT`).
    Not2,
    BitNeg,
    UnaryMinus,
}

impl Op {
    pub fn restore(self) -> &'static str {
        match self {
            Op::Plus => "+",
            Op::Minus => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::IntDiv => "DIV",
            Op::Mod => "%",
            Op::Eq => "=",
            Op::NullSafeEq => "<=>",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::LogicAnd => "AND",
            Op::LogicOr => "OR",
            Op::LogicXor => "XOR",
            Op::LeftShift => "<<",
            Op::RightShift => ">>",
            Op::Not => "NOT",
            Op::Not2 => "!",
            Op::BitNeg => "~",
            Op::UnaryMinus => "-",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    Plus,
    Minus,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogicAnd,
    LogicOr,
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
            Op::Mod => "%",
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::LogicAnd => "AND",
            Op::LogicOr => "OR",
            Op::Not => "NOT",
            Op::Not2 => "!",
            Op::BitNeg => "~",
            Op::UnaryMinus => "-",
        }
    }
}

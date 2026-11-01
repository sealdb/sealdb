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
            Op::BitNeg => "~",
            Op::UnaryMinus => "-",
        }
    }
}

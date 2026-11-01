#![forbid(unsafe_code)]

pub mod builtin_arithmetic;
pub mod builtin_cast;
pub mod builtin_compare;
pub mod builtin_control;
pub mod builtin_op;
pub mod constant;
pub mod evaluator;

pub use evaluator::eval_expr;

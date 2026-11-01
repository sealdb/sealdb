/// Arithmetic builtins are evaluated in `eval.rs` for F01.
pub fn supported() -> &'static [&'static str] {
    &["+", "-", "*", "/", "%"]
}

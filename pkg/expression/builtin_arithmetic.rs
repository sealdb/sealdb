/// Arithmetic builtins are evaluated in `eval.rs` for .
pub fn supported() -> &'static [&'static str] {
    &["+", "-", "*", "/", "%"]
}

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CIStr {
    #[serde(rename = "O")]
    pub o: String,
    #[serde(rename = "L")]
    pub l: String,
}

impl CIStr {
    pub fn new(s: impl Into<String>) -> Self {
        let o = s.into();
        let l = o.to_ascii_lowercase();
        Self { o, l }
    }

    pub fn o(&self) -> &str {
        &self.o
    }

    pub fn l(&self) -> &str {
        &self.l
    }
}

impl Default for CIStr {
    fn default() -> Self {
        Self::new("")
    }
}

impl From<&str> for CIStr {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_ci_str_lowercases() {
        let name = CIStr::new("TestDB");
        assert_eq!(name.o, "TestDB");
        assert_eq!(name.l, "testdb");
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#"{"O":"TestDB","L":"testdb"}"#);
    }
}

//! unicode_ci collator compare/key — TiDB `unicode_0400_ci_generated.go`.

use super::unicode_0400_ci_impl::{get_weight, preprocess};
use std::cmp::Ordering;

fn sign(v: i64) -> Ordering {
    v.cmp(&0)
}

/// Compare `a` and `b` under utf8mb4_unicode_ci / utf8_unicode_ci weight rules.
pub fn compare(a: &str, b: &str) -> Ordering {
    let a = preprocess(a);
    let b = preprocess(b);

    let mut an = 0u64;
    let mut bn = 0u64;
    let mut as_ = 0u64;
    let mut bs = 0u64;
    let mut ai = 0usize;
    let mut bi = 0usize;

    loop {
        if an == 0 {
            if as_ == 0 {
                while an == 0 && ai < a.len() {
                    let ch = a[ai..].chars().next().expect("ai in bounds");
                    ai += ch.len_utf8();
                    let (n, s) = get_weight(ch);
                    an = n;
                    as_ = s;
                }
            } else {
                an = as_;
                as_ = 0;
            }
        }

        if bn == 0 {
            if bs == 0 {
                while bn == 0 && bi < b.len() {
                    let ch = b[bi..].chars().next().expect("bi in bounds");
                    bi += ch.len_utf8();
                    let (n, s) = get_weight(ch);
                    bn = n;
                    bs = s;
                }
            } else {
                bn = bs;
                bs = 0;
            }
        }

        if an == 0 || bn == 0 {
            return sign(an as i64 - bn as i64);
        }

        if an == bn {
            an = 0;
            bn = 0;
            continue;
        }

        while an != 0 && bn != 0 {
            if (an ^ bn) & 0xFFFF != 0 {
                return sign((an & 0xFFFF) as i64 - (bn & 0xFFFF) as i64);
            }
            an >>= 16;
            bn >>= 16;
        }
    }
}

/// Equality under unicode_ci weights.
pub fn equal(a: &str, b: &str) -> bool {
    compare(a, b) == Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_and_accent_insensitive() {
        assert!(equal("A", "a"));
        assert!(equal("ä", "A"));
        assert_eq!(compare("a", "b"), Ordering::Less);
        assert_eq!(compare("B", "a"), Ordering::Greater);
    }

    #[test]
    fn sharp_s_equals_ss() {
        assert!(equal("ß", "ss"));
        assert!(equal("ß", "SS"));
        assert!(!equal("ß", "s"));
    }

    #[test]
    fn trailing_space_pad() {
        assert!(equal("a", "a  "));
    }
}

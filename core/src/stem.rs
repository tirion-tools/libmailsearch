//! Snowball stemming for query-time stem expansion: the reader stems a
//! query word and keeps the vocabulary terms whose stem is equal. The index holds no stems.

use rust_stemmers::{Algorithm, Stemmer};

/// Snowball stemmers by language code (`en` always; others when the caller enables them).
pub struct Snowball {
    langs: Vec<(&'static str, Stemmer)>,
}

/// Languages Snowball is used for, by code.
pub const LANGS: [(&str, Algorithm); 16] = [
    ("en", Algorithm::English),
    ("de", Algorithm::German),
    ("fr", Algorithm::French),
    ("es", Algorithm::Spanish),
    ("ru", Algorithm::Russian),
    ("it", Algorithm::Italian),
    ("pt", Algorithm::Portuguese),
    ("nl", Algorithm::Dutch),
    ("sv", Algorithm::Swedish),
    ("no", Algorithm::Norwegian),
    ("da", Algorithm::Danish),
    ("fi", Algorithm::Finnish),
    ("hu", Algorithm::Hungarian),
    ("ro", Algorithm::Romanian),
    ("tr", Algorithm::Turkish),
    ("el", Algorithm::Greek),
];

impl Snowball {
    /// English plus every known language of `extra` (unknown codes are ignored).
    pub fn new(extra: &[&str]) -> Snowball {
        let mut langs = vec![("en", Stemmer::create(Algorithm::English))];
        for code in extra {
            if let Some((c, a)) = LANGS.iter().find(|(c, _)| c == code)
                && !langs.iter().any(|(l, _)| l == c)
            {
                langs.push((c, Stemmer::create(*a)));
            }
        }
        Snowball { langs }
    }

    /// `(language, stem)` of `word` for every enabled language.
    pub fn stems<'a>(&'a self, word: &'a str) -> impl Iterator<Item = (&'a Stemmer, String)> + 'a {
        self.langs
            .iter()
            .map(move |(_, s)| (s, s.stem(word).into_owned()))
    }
}

/// A word eligible for the stem and typo tiers: ≥ 4 characters, all alphabetic.
pub fn eligible(t: &str) -> bool {
    t.chars().count() >= 4 && t.chars().all(|c| c.is_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_stems() {
        let s = Snowball::new(&["de", "xx", "en"]);
        let v: Vec<String> = s.stems("invoices").map(|x| x.1).collect();
        assert_eq!(v, vec!["invoic", "invoic"]);
        assert!(eligible("mail") && !eligible("ab1c") && !eligible("abc"));
    }
}

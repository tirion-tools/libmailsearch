//! Snowball stemming for query-time stem expansion: the reader stems a
//! query word and keeps the vocabulary terms whose stem is equal. The index holds no stems.
//! The stemmers are frostem's, the Snowball build Tantivy's own stemmer uses.

use frostem::{Algorithm, Stemmer};

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
        let mut langs = vec![("en", Stemmer::new(Algorithm::English))];
        for code in extra {
            if let Some((c, a)) = LANGS.iter().find(|(c, _)| c == code)
                && !langs.iter().any(|(l, _)| l == c)
            {
                langs.push((c, Stemmer::new(*a)));
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

    /// Every language stems words of every script without panicking (rust-stemmers 1.2.0's Greek stemmer panicked on
    /// multibyte suffixes, e.g. "αντιθετε").
    #[test]
    fn every_language_stems_any_word() {
        let codes: Vec<&str> = LANGS.iter().map(|(c, _)| *c).collect();
        let s = Snowball::new(&codes);
        assert_eq!(s.langs.len(), LANGS.len());
        let words = [
            "",
            "a",
            "invoices",
            "zahlungen",
            "factures",
            "facturas",
            "счета",
            "fatture",
            "faturas",
            "facturen",
            "fakturor",
            "fakturaer",
            "laskut",
            "számlák",
            "facturi",
            "faturalar",
            "τιμολόγια",
            "αντιθετε",
            "会議室",
            "ﬁnance",
            "e\u{301}te\u{301}",
            "\u{200b}",
            "ǅ",
            "İstanbul",
            "ß",
        ];
        for w in words {
            let stems: Vec<String> = s.stems(w).map(|x| x.1).collect();
            assert_eq!(stems.len(), LANGS.len(), "{w}");
        }
        let el = Snowball::new(&["el"]);
        assert_eq!(el.stems("αντιθετε").nth(1).unwrap().1, "ανετ");
    }
}

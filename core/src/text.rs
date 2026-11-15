//! Tokenizers and query-token rules: script routing of unsegmented
//! scripts, the `mail` word analyzer, the `cjk2` bigram tokenizer, 1–3-letter edge prefixes
//! (`hpfx`), and the query compiler's identifier → phrase rule.

use crate::norm::n1;
use std::borrow::Cow;
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, Token,
    TokenStream, Tokenizer,
};

/// Tokens longer than this many bytes are dropped by the `mail` analyzer.
pub const MAX_TOKEN_BYTES: usize = 65;

/// Unsegmented scripts routed to the `ng_*` bigram fields: Hiragana, Katakana (+ phonetic
/// extensions, halfwidth forms), Han (CJK unified ideographs, extensions A–H, compatibility
/// ideographs, iteration marks), Hangul (syllables and jamo), Thai, Lao, Khmer, Myanmar.
pub fn is_cjk(cp: u32) -> bool {
    matches!(
        cp,
        0x0E00..=0x0EFF // Thai, Lao
            | 0x1000..=0x109F // Myanmar
            | 0x1100..=0x11FF // Hangul jamo
            | 0x1780..=0x17FF // Khmer
            | 0x3005..=0x3007 // 々 〆 〇
            | 0x3040..=0x30FF // Hiragana, Katakana
            | 0x3130..=0x318F // Hangul compatibility jamo
            | 0x31F0..=0x31FF // Katakana phonetic extensions
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F // Hangul jamo extended-A
            | 0xAA60..=0xAA7F // Myanmar extended-A
            | 0xAC00..=0xD7AF // Hangul syllables
            | 0xD7B0..=0xD7FF // Hangul jamo extended-B
            | 0xF900..=0xFAFF
            | 0xFF66..=0xFF9F
            | 0x20000..=0x323AF
    )
}

pub fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| is_cjk(c as u32))
}

/// Blanks unsegmented-script characters (each replaced by as many spaces as it has UTF-8 bytes, so
/// byte offsets are kept): the word tokenizer treats them as alphanumeric, they are indexed only as
/// bigrams.
pub fn strip_cjk(s: &str) -> Cow<'_, str> {
    if !has_cjk(s) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_cjk(c as u32) {
            for _ in 0..c.len_utf8() {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// The `mail` word analyzer: split at non-alphanumerics, drop tokens > 65 bytes, lowercase, fold
/// Latin diacritics. N1 and [`strip_cjk`] are applied by the caller.
pub fn mail_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

/// Word tokens of already normalised text (CJK blanked here).
pub fn word_tokens_normalised(an: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let s = strip_cjk(text);
    let mut ts = an.token_stream(&s);
    let mut v = Vec::new();
    while ts.advance() {
        v.push(ts.token().text.clone());
    }
    v
}

/// Word-field tokens of raw text: N1, CJK blanked, `mail` analyzer.
pub fn word_tokens(text: &str) -> Vec<String> {
    word_tokens_normalised(&mut mail_analyzer(), &n1(text))
}

/// Overlapping bigrams of every unsegmented-script run of already normalised text, a lone
/// character as a unigram (the `cjk2` tokenizer's terms).
pub fn bigram_tokens(text: &str) -> Vec<String> {
    let mut ts = CjkBigram.token_stream(text);
    let mut v = Vec::new();
    while ts.advance() {
        v.push(ts.token().text.clone());
    }
    v
}

/// The unsegmented-script runs of a word and the remainder (runs replaced by a space).
pub fn split_cjk(word: &str) -> (Vec<String>, String) {
    let mut runs = Vec::new();
    let mut rest = String::new();
    let mut cur = String::new();
    for c in word.chars() {
        if is_cjk(c as u32) {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                runs.push(std::mem::take(&mut cur));
                rest.push(' ');
            }
            rest.push(c);
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    (runs, rest.trim().to_string())
}

/// The distinct 1..=`max`-character prefixes of every word token of `texts` (normalised text),
/// space separated (the `hpfx` field).
pub fn edge_prefixes(an: &mut TextAnalyzer, texts: &[&str], max: usize) -> String {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = String::new();
    for t in texts {
        let s = strip_cjk(t);
        let mut ts = an.token_stream(&s);
        while ts.advance() {
            let w = &ts.token().text;
            for (l, (i, c)) in w.char_indices().enumerate() {
                if l >= max {
                    break;
                }
                let p = &w[..i + c.len_utf8()];
                if seen.insert(p.to_string()) {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(p);
                }
            }
        }
    }
    out
}

/// A compiled query word: one token stays a term; a word the tokenizer splits
/// (`alice@example.com`, `INV-2024-0042`) becomes a phrase of its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryToken {
    Term(String),
    Phrase(Vec<String>),
}

impl std::fmt::Display for QueryToken {
    /// `term` or `"a b c"` (the notation of `tests/vectors/query.tsv`).
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            QueryToken::Term(t) => f.write_str(t),
            QueryToken::Phrase(p) => write!(f, "\"{}\"", p.join(" ")),
        }
    }
}

/// One query word compiled: the bigrams of each unsegmented-script run, and the word part.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompiledWord {
    pub cjk: Vec<Vec<String>>,
    pub word: Option<QueryToken>,
}

/// Compiles one raw query word (or phrase text): N1, CJK runs → bigram lists, the rest → a term
/// or a phrase of its parts.
pub fn compile_word(an: &mut TextAnalyzer, raw: &str) -> CompiledWord {
    let w = n1(raw);
    let (runs, rest) = if has_cjk(&w) {
        split_cjk(&w)
    } else {
        (Vec::new(), w)
    };
    let cjk = runs.iter().map(|r| bigram_tokens(r)).collect();
    let mut parts = word_tokens_normalised(an, &rest);
    let word = match parts.len() {
        0 => None,
        1 => parts.pop().map(QueryToken::Term),
        _ => Some(QueryToken::Phrase(parts)),
    };
    CompiledWord { cjk, word }
}

// ---- cjk2 tokenizer ----------------------------------------------------------------------------

/// Overlapping bigrams of every run of unsegmented-script characters, a lone character as a
/// unigram; everything else is skipped. Positions are consecutive inside a run and jump by 2
/// between runs, so a phrase of bigrams never spans two runs.
#[derive(Clone, Default)]
pub struct CjkBigram;

pub struct CjkStream {
    toks: Vec<Token>,
    i: usize,
}

impl Tokenizer for CjkBigram {
    type TokenStream<'a> = CjkStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> CjkStream {
        let mut toks = Vec::new();
        let mut pos = 0usize;
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let end_of = |k: usize| {
            if k < chars.len() {
                chars[k].0
            } else {
                text.len()
            }
        };
        let mut i = 0;
        while i < chars.len() {
            if !is_cjk(chars[i].1 as u32) {
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && is_cjk(chars[i].1 as u32) {
                i += 1;
            }
            let run = &chars[start..i];
            if run.len() == 1 {
                toks.push(Token {
                    offset_from: run[0].0,
                    offset_to: end_of(start + 1),
                    position: pos,
                    text: run[0].1.to_string(),
                    position_length: 1,
                });
                pos += 1;
            } else {
                for k in 0..run.len() - 1 {
                    let mut s = String::with_capacity(8);
                    s.push(run[k].1);
                    s.push(run[k + 1].1);
                    toks.push(Token {
                        offset_from: run[k].0,
                        offset_to: end_of(start + k + 2),
                        position: pos,
                        text: s,
                        position_length: 1,
                    });
                    pos += 1;
                }
            }
            pos += 1;
        }
        CjkStream { toks, i: 0 }
    }
}

impl TokenStream for CjkStream {
    fn advance(&mut self) -> bool {
        if self.i < self.toks.len() {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn token(&self) -> &Token {
        &self.toks[self.i - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.toks[self.i - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_keeps_offsets_and_compile() {
        assert_eq!(strip_cjk("a会b").len(), "a会b".len());
        let mut an = mail_analyzer();
        let c = compile_word(&mut an, "会議-INV2");
        assert_eq!(c.cjk, vec![vec!["会議".to_string()]]);
        assert_eq!(c.word, Some(QueryToken::Term("inv2".into())));
        assert_eq!(compile_word(&mut an, "--").word, None);
        assert_eq!(
            edge_prefixes(&mut an, &["Ab abc", "x"], 3),
            "a ab abc x".to_string()
        );
        assert_eq!(bigram_tokens("ไทย"), vec!["ไท", "ทย"]);
    }
}

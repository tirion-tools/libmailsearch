//! `mailsearch_highlight`: byte spans of a text that a query's positive clauses match, with the indexing
//! normalisation and tokenizers (N1 with an offset map, `mail` words, `cjk2` bigrams).

use crate::engine::{HlTerm, T_EXACT, T_PREFIX, T_STEM, T_TYPO};
use mailsearch_core::norm::{map_back, n1_mapped};
use mailsearch_core::stem::Snowball;
use mailsearch_core::text::{CjkBigram, strip_cjk};
use tantivy::tokenizer::{TextAnalyzer, TokenStream, Tokenizer};

struct Tok {
    text: String,
    from: usize,
    to: usize,
    pos: usize,
}

fn collect(mut ts: impl TokenStream) -> Vec<Tok> {
    let mut v = Vec::new();
    while ts.advance() {
        let t = ts.token();
        v.push(Tok {
            text: t.text.clone(),
            from: t.offset_from,
            to: t.offset_to,
            pos: t.position,
        });
    }
    v
}

/// Optimal string alignment distance ≤ 1 (one insertion, deletion, substitution or adjacent
/// transposition), as the typo tier's `FuzzyTermQuery(distance 1, transpositions)`.
fn within_one(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a == b {
        return true;
    }
    let (la, lb) = (a.len(), b.len());
    if la.abs_diff(lb) > 1 {
        return false;
    }
    let p = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    if la == lb {
        // substitution or transposition at p
        a[p + 1..] == b[p + 1..]
            || (p + 1 < la && a[p] == b[p + 1] && a[p + 1] == b[p] && a[p + 2..] == b[p + 2..])
    } else if la > lb {
        a[p + 1..] == b[p..]
    } else {
        a[p..] == b[p + 1..]
    }
}

/// Matched byte spans `(start, end, tier)` of `text`, sorted and merged (an overlap keeps the best
/// tier: the lowest MAILSEARCH_T_* bit).
pub(crate) fn spans(
    an: &mut TextAnalyzer,
    snow: &Snowball,
    terms: &[HlTerm],
    typing: bool,
    text: &str,
) -> Vec<(u32, u32, u32)> {
    let (norm, map) = n1_mapped(text);
    let blanked = strip_cjk(&norm);
    let words = collect(an.token_stream(&blanked));
    let bigrams = collect(CjkBigram.token_stream(&norm));
    let mut out: Vec<(usize, usize, u32)> = Vec::new();
    let seq =
        |toks: &[Tok], parts: &[String], last_prefix: bool, out: &mut Vec<(usize, usize, u32)>| {
            let n = parts.len();
            if n == 0 || toks.len() < n {
                return;
            }
            for i in 0..=toks.len() - n {
                let ok = (0..n).all(|k| {
                    let t = &toks[i + k];
                    let p = &parts[k];
                    (k == 0 || t.pos == toks[i + k - 1].pos + 1)
                        && (t.text == *p
                            || (last_prefix && k == n - 1 && t.text.starts_with(p.as_str())))
                });
                if ok {
                    let exact = toks[i + n - 1].text == parts[n - 1];
                    out.push((
                        toks[i].from,
                        toks[i + n - 1].to,
                        if exact { T_EXACT } else { T_PREFIX },
                    ));
                }
            }
        };
    for term in terms {
        match term {
            HlTerm::Word { t, prefix, stem } => {
                let stems: Vec<_> = if *stem {
                    snow.stems(t).collect()
                } else {
                    Vec::new()
                };
                let before = out.len();
                for w in &words {
                    let tier = if w.text == *t {
                        T_EXACT
                    } else if *prefix && w.text.starts_with(t) {
                        T_PREFIX
                    } else if stems.iter().any(|(s, st)| s.stem(&w.text) == st.as_str()) {
                        T_STEM
                    } else {
                        continue;
                    };
                    out.push((w.from, w.to, tier));
                }
                // the typo tier only when the word itself is absent (pause/Enter only)
                if *stem && !typing && out.len() == before {
                    for w in &words {
                        if within_one(&w.text, t) {
                            out.push((w.from, w.to, T_TYPO));
                        }
                    }
                }
            }
            HlTerm::Phrase { parts, last_prefix } => seq(&words, parts, *last_prefix, &mut out),
            HlTerm::Cjk { bigrams: b } => seq(&bigrams, b, false, &mut out),
        }
    }
    let mut v: Vec<(usize, usize, u32)> = out
        .into_iter()
        .map(|(a, b, t)| {
            let (x, y) = map_back(&map, text, a, b);
            (x, y, t)
        })
        .collect();
    v.sort_unstable();
    let mut merged: Vec<(u32, u32, u32)> = Vec::with_capacity(v.len());
    for (a, b, t) in v {
        if let Some(last) = merged.last_mut()
            && a < last.1 as usize
        {
            last.1 = last.1.max(b as u32);
            last.2 = last.2.min(t);
            continue;
        }
        merged.push((a as u32, b as u32, t));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::within_one;

    #[test]
    fn distance_one() {
        assert!(within_one("recieve", "receive"));
        assert!(within_one("invoice", "invoce"));
        assert!(within_one("invoice", "invoices"));
        assert!(within_one("invoice", "invoixe"));
        assert!(!within_one("invoice", "involve"));
        assert!(!within_one("abc", "cab"));
    }
}

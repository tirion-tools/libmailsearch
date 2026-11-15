//! Text rules applied before the tokenizers: normalisation N1 (index and query side), link hosts
//! and digit groups.

use std::borrow::Cow;
use unicode_normalization::UnicodeNormalization;

fn is_format(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x061C
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
    )
}

fn is_greek(c: char) -> bool {
    matches!(c as u32, 0x0370..=0x03FF | 0x1F00..=0x1FFF)
}

/// N1: NFKC; strip Greek tonos/dialytika, Arabic harakat and tatweel, Hebrew points; ё→е, ß→ss;
/// drop invisible format characters and bidi controls. Case is preserved.
pub fn n1(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfkc() {
        let cp = c as u32;
        if is_format(c)
            || (0x064B..=0x065F).contains(&cp)
            || cp == 0x0670
            || cp == 0x0640
            || (0x0591..=0x05C7).contains(&cp)
        {
            continue;
        }
        match c {
            'ё' => out.push('е'),
            'Ё' => out.push('Е'),
            'ß' => out.push_str("ss"),
            'ẞ' => out.push_str("SS"),
            _ if is_greek(c) => {
                unicode_normalization::char::decompose_canonical(c, |d| {
                    if !matches!(d as u32, 0x0300..=0x036F) {
                        out.push(d)
                    }
                });
            }
            _ => out.push(c),
        }
    }
    out
}

/// N1 without a copy for ASCII text (N1 only changes text with non-ASCII characters).
pub fn n1_cow(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(n1(s))
    }
}

/// N1 with a byte-offset map back to `s` (highlighting): `map` holds `(normalised_offset,
/// original_offset)` at every chunk boundary, ascending, ending with `(out.len(), s.len())`. A chunk
/// is a starter with its following combining marks (and Hangul medial/final jamo), normalised on
/// its own, so composition never crosses a chunk.
pub fn n1_mapped(s: &str) -> (String, Vec<(usize, usize)>) {
    if s.is_ascii() {
        return (s.to_string(), vec![(0, 0), (s.len(), s.len())]);
    }
    let joins = |c: char| {
        unicode_normalization::char::canonical_combining_class(c) != 0
            || matches!(c as u32, 0x1160..=0x11FF | 0xD7B0..=0xD7FF)
    };
    let mut out = String::with_capacity(s.len());
    let mut map = vec![(0, 0)];
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((_, c)) = it.next() {
        let mut end = start + c.len_utf8();
        while let Some(&(i, d)) = it.peek() {
            if !joins(d) {
                break;
            }
            end = i + d.len_utf8();
            it.next();
        }
        out.push_str(&n1(&s[start..end]));
        map.push((out.len(), end));
        start = end;
    }
    (out, map)
}

/// Maps a byte range of [`n1_mapped`]`(s)`'s output back to `s`: offsets inside a chunk N1 left the
/// same length (unchanged text, e.g. all of an ASCII input) map exactly; otherwise the range widens
/// to whole chunks.
pub fn map_back(map: &[(usize, usize)], s: &str, from: usize, to: usize) -> (usize, usize) {
    let at = |off: usize, start: bool| -> usize {
        match map.binary_search_by(|x| x.0.cmp(&off)) {
            Ok(i) => map[i].1,
            Err(i) if i == 0 || i >= map.len() => map[i.min(map.len() - 1)].1,
            Err(i) => {
                let (n0, o0) = map[i - 1];
                let (n1, o1) = map[i];
                let inside = o0 + (off - n0);
                if n1 - n0 == o1 - o0 && s.is_char_boundary(inside) {
                    inside
                } else if start {
                    o0
                } else {
                    o1
                }
            }
        }
    };
    (at(from, true), at(to, false))
}

/// Hosts of `http(s)://` and `www.` links, lowercased, space separated, in order of appearance.
pub fn link_hosts(text: &str) -> String {
    let mut out = String::new();
    link_hosts_into(&mut out, text);
    out
}

/// [`link_hosts`] appended to `out` (space separated from what `out` holds).
pub fn link_hosts_into(out: &mut String, text: &str) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let rest = &text[i..];
        let skip = if rest.starts_with("http://") {
            7
        } else if rest.starts_with("https://") {
            8
        } else if rest.starts_with("www.") && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            0
        } else {
            i += 1;
            while i < b.len() && !text.is_char_boundary(i) {
                i += 1;
            }
            continue;
        };
        let start = i + skip;
        let mut j = start;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'-') {
            j += 1;
        }
        if j > start {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&text[start..j].to_ascii_lowercase());
        }
        i = j.max(i + 1);
        while i < b.len() && !text.is_char_boundary(i) {
            i += 1;
        }
    }
}

/// Digit groups separated by one of ` -./()` (or `) `) forming 6–15 digits in total, concatenated,
/// space separated (the `numbers` field). A single run of 6–15 digits is written too
/// (`tests/vectors/numbers.tsv` `six_digits`, `fifteen_digits`); a run preceded by a letter or
/// digit is not the start of a group.
pub fn number_groups(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut digits = String::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && b[i - 1].is_ascii_alphanumeric()) {
            i += 1;
            continue;
        }
        digits.clear();
        let mut j = i;
        loop {
            let s = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j == s {
                break;
            }
            digits.push_str(&text[s..j]);
            if j + 1 < b.len() && b" -./()".contains(&b[j]) && b[j + 1].is_ascii_digit() {
                j += 1;
                continue;
            }
            if j + 2 < b.len() && b[j] == b')' && b[j + 1] == b' ' && b[j + 2].is_ascii_digit() {
                j += 2;
                continue;
            }
            break;
        }
        if (6..=15).contains(&digits.len()) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&digits);
        }
        i = j.max(i + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_offsets() {
        let s = "x Mu\u{0308}ller ＡＢ";
        let (n, m) = n1_mapped(s);
        assert_eq!(n, n1(s));
        let i = n.find("Müller").unwrap();
        let (a, b) = map_back(&m, s, i, i + "Müller".len());
        assert_eq!(&s[a..b], "Mu\u{0308}ller");
        let i = n.find("AB").unwrap();
        assert_eq!(map_back(&m, s, i, i + 2), (s.len() - 6, s.len()));
        let s = "mail alice@example.com";
        let (_, m) = n1_mapped(s);
        assert_eq!(map_back(&m, s, 5, 10), (5, 10));
    }

    #[test]
    fn hosts() {
        assert_eq!(
            link_hosts("see https://Example.ORG/x and www.foo-bar.com, http://"),
            "example.org www.foo-bar.com"
        );
        assert_eq!(link_hosts("awww.x.com café https://ü.de"), "");
    }

    #[test]
    fn numbers_inside_letters_are_skipped() {
        assert_eq!(number_groups("INV1234567"), "");
        assert_eq!(
            number_groups("a 555-123-4567 b 12345678"),
            "5551234567 12345678"
        );
    }
}

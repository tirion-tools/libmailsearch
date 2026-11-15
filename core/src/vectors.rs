//! Loader for the shared test vectors in `schema/vectors/` (format in `FORMAT.txt`).

use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Spec,
    Inferred,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub id: String,
    pub status: Status,
    /// Decoded columns after `id` and `status`; `None` for the empty marker `∅`.
    pub fields: Vec<Option<String>>,
}

#[derive(Debug)]
pub struct VectorFile {
    pub norm_version: u32,
    pub cases: Vec<Case>,
}

/// Decodes `\u{XXXX}`, `\t` and `\\`.
pub fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                if it.next() != Some('{') {
                    return Err(format!("bad escape in {s:?}"));
                }
                let hex: String = it.by_ref().take_while(|&c| c != '}').collect();
                if hex.is_empty() || hex.len() > 6 {
                    return Err(format!("bad \\u{{}} in {s:?}"));
                }
                let v = u32::from_str_radix(&hex, 16).map_err(|e| format!("{s:?}: {e}"))?;
                out.push(char::from_u32(v).ok_or_else(|| format!("not a scalar value: {hex}"))?);
            }
            other => return Err(format!("bad escape \\{other:?} in {s:?}")),
        }
    }
    Ok(out)
}

/// Parses one vector file with exactly `columns` tab-separated columns per case.
pub fn parse(text: &str, columns: usize) -> Result<VectorFile, String> {
    let mut norm_version = None;
    let mut ids = HashSet::new();
    let mut cases = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            if let Some(v) = rest.trim().strip_prefix("norm_version:") {
                norm_version = Some(
                    v.trim()
                        .parse::<u32>()
                        .map_err(|e| format!("line {n}: {e}"))?,
                );
            }
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() != columns {
            return Err(format!(
                "line {n}: {} columns, expected {columns}",
                cols.len()
            ));
        }
        let id = cols[0];
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(format!("line {n}: bad id {id:?}"));
        }
        if !ids.insert(id.to_owned()) {
            return Err(format!("line {n}: duplicate id {id}"));
        }
        let status = match cols[1] {
            "spec" => Status::Spec,
            "inferred" => Status::Inferred,
            s => return Err(format!("line {n}: bad status {s:?}")),
        };
        let fields = cols[2..]
            .iter()
            .map(|f| {
                if *f == "∅" {
                    Ok(None)
                } else {
                    unescape(f).map(Some)
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("line {n}: {e}"))?;
        if fields.iter().flatten().any(|f| f.contains('\0')) {
            return Err(format!("line {n}: NUL in field"));
        }
        cases.push(Case {
            id: id.to_owned(),
            status,
            fields,
        });
    }
    let norm_version = norm_version.ok_or("missing `# norm_version:` line")?;
    if cases.is_empty() {
        return Err("no cases".into());
    }
    Ok(VectorFile {
        norm_version,
        cases,
    })
}

pub fn load(path: &Path, columns: usize) -> Result<VectorFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text, columns).map_err(|e| format!("{}: {e}", path.display()))
}

/// Every vector file, its column count and the rule it pins.
pub const FILES: [(&str, usize); 5] = [
    ("normalisation.tsv", 4),
    ("words.tsv", 4),
    ("numbers.tsv", 4),
    ("segmentation.tsv", 5),
    ("query.tsv", 4),
];

fn joined(v: Vec<String>) -> String {
    v.join(" ")
}

/// What this crate's rules produce for one case of `file`: one string per expected column.
fn actual(file: &str, input: &str) -> Vec<String> {
    use crate::norm::{n1, number_groups};
    use crate::text::{bigram_tokens, compile_word, mail_analyzer, word_tokens};
    match file {
        "normalisation.tsv" => vec![n1(input)],
        "words.tsv" => vec![joined(word_tokens(input))],
        "numbers.tsv" => vec![number_groups(&n1(input))],
        "segmentation.tsv" => vec![
            joined(word_tokens(input)),
            joined(bigram_tokens(&n1(input))),
        ],
        "query.tsv" => vec![
            compile_word(&mut mail_analyzer(), input)
                .word
                .map(|w| w.to_string())
                .unwrap_or_default(),
        ],
        _ => unreachable!("unknown vector file {file}"),
    }
}

/// Rule-by-rule parity of every vector file in `dir` (`schema/vectors`) with this crate, shared by
/// the indexer's and the reader's tests. Checks the format, that every `.tsv` is covered, that each
/// file describes [`crate::NORM_VERSION`], and every case (`spec` and `inferred`). Returns the
/// number of cases checked, or every failure.
pub fn check_all(dir: &Path) -> Result<usize, String> {
    let mut fails = Vec::new();
    let mut n = 0;
    for (name, cols) in FILES {
        let f = load(&dir.join(name), cols)?;
        if f.norm_version != crate::NORM_VERSION {
            fails.push(format!(
                "{name}: norm_version {} != {}",
                f.norm_version,
                crate::NORM_VERSION
            ));
        }
        for c in &f.cases {
            n += 1;
            let input = c.fields[0].as_deref().unwrap_or("");
            let want: Vec<&str> = c.fields[1..]
                .iter()
                .map(|x| x.as_deref().unwrap_or(""))
                .collect();
            let got = actual(name, input);
            if got != want {
                fails.push(format!(
                    "{name} {} ({:?}): input {input:?}: got {got:?}, expected {want:?}",
                    c.id, c.status
                ));
            }
        }
    }
    let on_disk = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "tsv"))
        .filter(|e| !FILES.iter().any(|(n, _)| e.file_name() == *n))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if !on_disk.is_empty() {
        fails.push(format!("vector files without a checker: {on_disk:?}"));
    }
    if fails.is_empty() {
        Ok(n)
    } else {
        Err(fails.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schema/vectors")
    }

    /// Rule-by-rule parity with every shared vector (also the format and version check).
    #[test]
    fn shared_vectors_parity() {
        let n = check_all(&dir()).unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0);
    }

    #[test]
    fn escapes_and_format_errors() {
        assert_eq!(unescape(r"a\u{200B}b\tc\\").unwrap(), "a\u{200B}b\tc\\");
        assert!(unescape(r"\u{D800}").is_err());
        assert!(unescape(r"\x").is_err());
        assert!(parse("# norm_version: 1\nid\tspec\tonly\n", 4).is_err());
        assert!(parse("# norm_version: 1\na\tspec\tx\ty\na\tspec\tx\ty\n", 4).is_err());
        assert!(parse("a\tspec\tx\ty\n", 4).is_err());
        assert_eq!(
            parse("# norm_version: 1\na\tinferred\t∅\ty\n", 4)
                .unwrap()
                .cases[0]
                .fields[0],
            None
        );
    }
}

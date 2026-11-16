//! The corrupted-index harness shared by the `reader_index` fuzz target (fuzz/fuzz_targets) and the
//! regression test over its corpus (reader/tests/corrupt.rs). A small valid index is written once;
//! each input picks one of its files (byte 0), then rewrites it: byte 1 selects truncate-to-input,
//! overwrite-at-offset or full replacement with the rest of the input. The reader is driven
//! through the C ABI exactly as a search front end does (mailsearch_open, mailsearch_search typing
//! + pause, mailsearch_highlight, mailsearch_release, mailsearch_close).
#![allow(dead_code)]

use mailsearch::*;
use mailsearch_core::{DocInput, Fields, register, schema, stamp, to_tantivy};
use std::ffi::c_char;
use std::path::{Path, PathBuf};

pub struct Pristine {
    pub files: Vec<(PathBuf, Vec<u8>)>,
    pub work: PathBuf,
}

pub fn build() -> Pristine {
    let base = std::env::temp_dir().join(format!("mailsearch-fuzz-reader-{}", std::process::id()));
    let root = base.join("pristine");
    let _ = std::fs::remove_dir_all(&base);
    let mail = root.join("idx-1").join("mail");
    std::fs::create_dir_all(&mail).unwrap();
    let index = tantivy::Index::create_in_dir(&mail, schema()).unwrap();
    register(&index);
    let f = Fields::new(&index.schema()).unwrap();
    let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
    let words = [
        "invoice",
        "zephyr",
        "budget",
        "会議室",
        "555-123-4567",
        "müller",
        "plans",
    ];
    for mid in 1..=64u64 {
        let wd = words[(mid as usize) % words.len()];
        w.add_document(to_tantivy(
            &f,
            &DocInput {
                mid,
                search_key: ((1_700_000_000 + mid as i64 * 86_400) << 20) + mid as i64,
                subject: format!("{wd} subject {mid}"),
                body: format!("body {wd} text number {mid}"),
                quoted: format!("> quoted {wd}"),
                from: format!("Sender {mid} <s{mid}@example.org>"),
                from_h: mailsearch_core::fnv1a64(format!("s{mid}@example.org").as_bytes()),
                account: 1 + mid % 2,
                conv: mid / 3,
                msgid_h: mid % 5,
                ..Default::default()
            },
        ))
        .unwrap();
        if mid % 16 == 0 {
            w.commit().unwrap();
        }
    }
    w.commit().unwrap();
    std::fs::write(root.join("idx-1").join("VERSION"), stamp()).unwrap();
    std::fs::write(root.join("CURRENT"), "idx-1\n").unwrap();
    let mut files = Vec::new();
    collect(&root, &mut files);
    // Segment ids are random: order by the segment's position in meta.json, so byte 0 of an
    // input picks the same file in every run (the regression corpus depends on it).
    let meta = std::fs::read_to_string(mail.join("meta.json"))
        .unwrap()
        .replace('-', "");
    files.sort_by_key(|p| {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let seg = name.split('.').next().unwrap_or_default().to_string();
        let ord = match seg.len() {
            32 => meta.find(&seg).expect("segment listed in meta.json"),
            _ => 0,
        };
        let rest = name[seg.len()..].to_string();
        (p.parent().map(Path::to_path_buf), ord, rest)
    });
    let files = files
        .into_iter()
        .map(|p| {
            let b = std::fs::read(&p).unwrap();
            (p.strip_prefix(&root).unwrap().to_path_buf(), b)
        })
        .collect();
    Pristine {
        files,
        work: base.join("work"),
    }
}

fn collect(d: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(d).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            collect(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// `MAILSEARCH_F_TEXT` and `MAILSEARCH_M_DEFAULT` of mailsearch.h (the Rust crate exports no constants for them).
const F_TEXT: u32 = (1 << 11) - 1;
const M_DEFAULT: u32 = 0;

fn clause(text: &str, flags: u32) -> MailsearchClause {
    MailsearchClause {
        text: text.as_ptr() as *const c_char,
        len: text.len(),
        fields: F_TEXT,
        mode: M_DEFAULT,
        flags,
        group: 0,
        near: 0,
    }
}

/// Drives the reader over `root` as a search front end does; returns mailsearch_open's code.
pub fn run(root: &Path) -> i32 {
    let r = root.to_str().unwrap();
    let mut err = 0;
    // SAFETY: valid pointer/length pairs and out-pointers for every call below.
    unsafe {
        let g = mailsearch_open(r.as_ptr() as *const c_char, r.len(), &mut err);
        if g.is_null() {
            return err;
        }
        let mut top = [MailsearchHit {
            mid: 0,
            search_key: 0,
            score: 0.0,
            tiers: 0,
            fields: 0,
            copies: 0,
        }; 7];
        let mut list = [MailsearchHit {
            mid: 0,
            search_key: 0,
            score: 0.0,
            tiers: 0,
            fields: 0,
            copies: 0,
        }; 50];
        for (text, qflags, cflags) in [
            ("inv", MAILSEARCH_Q_TYPING, MAILSEARCH_C_PARTIAL),
            ("invoice zephyr", 0, 0),
            ("会議", 0, 0),
            ("budgte", MAILSEARCH_Q_RELEVANCE, 0),
        ] {
            let c = [clause(text, cflags)];
            let q = MailsearchQuery {
                clauses: c.as_ptr(),
                n_clauses: 1,
                now: 1_700_000_000 + 100 * 86_400,
                date_lo: 0,
                date_hi: 0,
                hint_lo: 0,
                hint_hi: 0,
                before_key: 0,
                account: 0,
                flags: qflags,
                n_top: 5,
            };
            let mut res = MailsearchResult {
                top: top.as_mut_ptr(),
                top_cap: top.len(),
                n_top: 0,
                list: list.as_mut_ptr(),
                list_cap: list.len(),
                n_list: 0,
                count: 0,
                count_capped: 0,
                flags: 0,
            };
            if mailsearch_search(g, &q, &mut res) == MAILSEARCH_E_POISONED {
                break;
            }
            let text = b"invoice zephyr budget";
            let mut spans = [MailsearchSpan {
                start: 0,
                end: 0,
                tier: 0,
            }; 8];
            mailsearch_highlight(
                g,
                &q,
                text.as_ptr() as *const c_char,
                text.len(),
                spans.as_mut_ptr(),
                spans.len(),
            );
        }
        let mut bytes = 0;
        mailsearch_release(g, &mut bytes);
        mailsearch_close(g);
    }
    MAILSEARCH_OK
}

/// Restores the pristine index under `p.work` and corrupts one file as `data` says: byte 0 picks
/// the file, byte 1 the mode (truncate to the input length, overwrite at an offset, replace).
/// Returns the corrupted index root, or None for an input shorter than two bytes.
pub fn corrupt<'a>(p: &'a Pristine, data: &[u8]) -> Option<&'a Path> {
    if data.len() < 2 {
        return None;
    }
    let _ = std::fs::remove_dir_all(&p.work);
    for (rel, bytes) in &p.files {
        let dst = p.work.join(rel);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&dst, bytes).unwrap();
    }
    let (rel, orig) = &p.files[usize::from(data[0]) % p.files.len()];
    let rest = &data[2..];
    let new = match data[1] % 3 {
        0 => orig[..rest.len().min(orig.len())].to_vec(),
        1 => {
            let mut b = orig.clone();
            if rest.len() >= 4 && !b.is_empty() {
                let off =
                    u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize % b.len();
                for (i, v) in rest[4..].iter().enumerate() {
                    if let Some(x) = b.get_mut(off + i) {
                        *x = *v;
                    }
                }
            }
            b
        }
        _ => rest.to_vec(),
    };
    std::fs::write(p.work.join(rel), new).unwrap();
    Some(&p.work)
}

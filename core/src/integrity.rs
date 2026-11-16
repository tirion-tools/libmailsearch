//! Integrity check of the index files Tantivy decodes into heap structures when it opens a segment.
//!
//! Tantivy 0.27 trusts lengths it reads from these files. A corrupted doc store makes
//! `StoreReader::open` reserve memory for an on-disk count (2.7 PB in the reader fuzz finding
//! `fuzz/corpus/reader_index/regress-store-skip-index`), and the failed allocation aborts the
//! process: no unwind boundary can catch it. Every file Tantivy writes ends with a footer holding
//! the CRC32 of its body, which Tantivy checks only on request (`Index::validate_checksum`, every
//! file). Checking it for the four components a segment decodes at open (doc store, fast fields,
//! field norms, delete bitset) costs little: the schema stores no documents, and fast fields and
//! norms are a few bytes per document. The term dictionaries, postings and positions (nearly all of
//! an index) are not read at open and stay lazily paged.
//!
//! A checksum stops corruption (disk, partial writes), not a crafted index: a hostile writer can
//! compute it. A process that maps an index written by another process keeps its own crash guard.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tantivy::directory::footer::Footer;
use tantivy::directory::{FileSlice, OwnedBytes};
use tantivy::index::SegmentComponent;
use tantivy::{Directory, HasLen, Index, SegmentMeta};

/// Components whose files are checked before a segment opens.
const CHECKED: [SegmentComponent; 4] = [
    SegmentComponent::Store,
    SegmentComponent::FastFields,
    SegmentComponent::FieldNorms,
    SegmentComponent::Delete,
];

/// True for a segment file (`<segment>.store`, `.fast`, `.fieldnorm`, `<segment>.<opstamp>.del`)
/// whose footer CRC is checked before Tantivy decodes it.
pub fn checked_at_open(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(|e| SegmentComponent::try_from(e).ok())
        .is_some_and(|c| CHECKED.contains(&c))
}

/// Bytes of a checked file read at a time: a reader reads them with `pread` into a buffer below
/// glibc's mmap threshold, so the check neither maps the file nor allocates the whole body.
const CHUNK: usize = 64 << 10;

/// Checks the footer CRC32 of one complete index file, reading the body in [`CHUNK`] slices.
pub fn check_file(file: FileSlice) -> Result<(), String> {
    let (footer, body) = Footer::extract_footer(file).map_err(|e| e.to_string())?;
    let mut h = crc32fast::Hasher::new();
    let n = body.len();
    let mut o = 0;
    while o < n {
        let e = (o + CHUNK).min(n);
        h.update(
            body.slice(o..e)
                .read_bytes()
                .map_err(|e| e.to_string())?
                .as_slice(),
        );
        o = e;
    }
    let crc = h.finalize();
    if crc != footer.crc {
        return Err(format!(
            "checksum mismatch (footer {:08x}, contents {crc:08x})",
            footer.crc
        ));
    }
    Ok(())
}

/// The checked files of one segment.
pub fn checked_files(meta: &SegmentMeta) -> Vec<PathBuf> {
    CHECKED
        .iter()
        .filter(|c| **c != SegmentComponent::Delete || meta.has_deletes())
        .map(|c| meta.relative_path(c.clone()))
        .collect()
}

/// Checks every checked file of the committed segments of `index` (a writer calls it before it
/// opens segments). Errors name the file.
pub fn check_index(index: &Index) -> Result<(), String> {
    let dir = index.directory();
    for meta in index
        .searchable_segment_metas()
        .map_err(|e| e.to_string())?
    {
        for p in checked_files(&meta) {
            // atomic_read: the whole file (a managed directory's file handle stops before the footer).
            let bytes = dir
                .atomic_read(&p)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            check_file(FileSlice::new(Arc::new(OwnedBytes::new(bytes))))
                .map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with_docs(dir: &Path) -> Index {
        let index = Index::create_in_dir(dir, crate::schema()).unwrap();
        crate::register(&index);
        let f = crate::Fields::new(&index.schema()).unwrap();
        let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for mid in 1..=8u64 {
            w.add_document(crate::to_tantivy(
                &f,
                &crate::DocInput {
                    mid,
                    search_key: mid as i64,
                    subject: format!("subject {mid}"),
                    ..Default::default()
                },
            ))
            .unwrap();
        }
        w.commit().unwrap();
        w.delete_term(tantivy::Term::from_field_u64(f.mid, 3));
        w.commit().unwrap();
        index
    }

    #[test]
    fn detects_a_flipped_byte_in_every_checked_component() {
        let tmp = std::env::temp_dir().join(format!("mailsearch-integrity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let index = index_with_docs(&tmp);
        check_index(&index).unwrap();
        let metas = index.searchable_segment_metas().unwrap();
        let files: Vec<PathBuf> = metas.iter().flat_map(checked_files).collect();
        assert_eq!(files.len(), 4, "store, fast, fieldnorm and del: {files:?}");
        for p in files {
            assert!(checked_at_open(&p), "{}", p.display());
            let full = tmp.join(&p);
            let orig = std::fs::read(&full).unwrap();
            let mut bad = orig.clone();
            bad[0] ^= 0x40;
            std::fs::write(&full, &bad).unwrap();
            let e = check_index(&index).unwrap_err();
            assert!(e.contains("checksum mismatch"), "{e}");
            std::fs::write(&full, &orig).unwrap();
        }
        check_index(&index).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn lazily_read_components_are_not_checked() {
        for name in [
            "a.idx",
            "a.pos",
            "a.term",
            "meta.json",
            ".managed.json",
            "VERSION",
        ] {
            assert!(!checked_at_open(Path::new(name)), "{name}");
        }
        for name in ["a.store", "a.fast", "a.fieldnorm", "a.12.del"] {
            assert!(checked_at_open(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn short_or_footerless_files_are_refused() {
        let file = |v: Vec<u8>| FileSlice::new(Arc::new(OwnedBytes::new(v)));
        assert!(check_file(file(Vec::new())).is_err());
        assert!(check_file(file(vec![0u8; 64])).is_err());
    }
}

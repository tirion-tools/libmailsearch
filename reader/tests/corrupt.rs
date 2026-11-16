//! Replays the `reader_index` fuzz corpus (fuzz/corpus/reader_index) through the fuzz harness: every
//! input must come back through the C ABI (no abort, no crash), and the two findings must be refused as
//! corrupt before Tantivy decodes the file: the doc-store skip index (`regress-store-skip-index`,
//! Tantivy reserves 2^51 bytes) and the fast-field column directory (`regress-fast-sstable-range`, the
//! sstable range reader grows a vector to 8 GiB; libFuzzer's "oom").

mod corrupt_index;

use mailsearch::{MAILSEARCH_E_CORRUPT, MAILSEARCH_OK};
use std::path::PathBuf;

fn corpus() -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fuzz/corpus/reader_index");
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn corpus_inputs_come_back_through_the_abi() {
    let p = corrupt_index::build();
    let inputs = corpus();
    assert!(
        ["regress-store-skip-index", "regress-fast-sstable-range"]
            .iter()
            .all(|r| inputs.iter().any(|(n, _)| n == r)),
        "corpus incomplete"
    );
    for (name, data) in inputs {
        let root = corrupt_index::corrupt(&p, &data).expect("inputs are at least two bytes");
        let code = corrupt_index::run(root);
        if name.starts_with("regress-") {
            assert_eq!(code, MAILSEARCH_E_CORRUPT, "{name}");
        }
    }
    // The pristine index still opens.
    let root = corrupt_index::corrupt(&p, &[0, 1]).unwrap();
    assert_eq!(corrupt_index::run(root), MAILSEARCH_OK);
}

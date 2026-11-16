//! Fuzzes libmailsearch on corrupted index files (harness: reader/tests/corrupt_index). A panic is
//! caught by the ABI and returned as MAILSEARCH_E_PANIC, which is the contract; a crash, abort,
//! hang or sanitizer report is a finding. Regression inputs: fuzz/corpus/reader_index (replayed by
//! reader/tests/corrupt.rs).
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

#[path = "../../reader/tests/corrupt_index/mod.rs"]
mod corrupt_index;

fuzz_target!(|data: &[u8]| {
    static P: OnceLock<corrupt_index::Pristine> = OnceLock::new();
    let p = P.get_or_init(|| {
        let p = corrupt_index::build();
        // The ABI's contract turns panics into MAILSEARCH_E_PANIC; libfuzzer-sys' hook would abort first.
        std::panic::set_hook(Box::new(|_| {}));
        p
    });
    if let Some(root) = corrupt_index::corrupt(p, data) {
        corrupt_index::run(root);
    }
});

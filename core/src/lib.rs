//! Shared search rules, linked by the index writer and by the reader library (`mailsearch`): normalisation N1, the `mail` word and `cjk2` bigram
//! tokenizers, digit groups and link hosts, the query-token rules (identifier → phrase of parts,
//! CJK split), the Snowball wrapper, the index layout with its `VERSION` stamp, and the hashes both
//! sides must agree on. The rules are pinned by `tests/vectors/` ([`vectors::check_all`]).
//!
//! Writer API: [`schema`], [`register`], [`Fields`], [`DocInput`] →
//! [`to_tantivy`]; [`people_schema`], [`PeopleFields`], [`PersonInput`] / [`CorrInput`] →
//! [`person_doc`] / [`corr_doc`], [`people_term`]; [`stamp`] / [`check_stamp`]; [`read_current`],
//! [`idx_name`]; [`integrity::check_index`] before a writer opens segments.

pub mod integrity;
pub mod layout;
pub mod norm;
pub mod stem;
pub mod text;
pub mod vectors;

pub use layout::{
    CorrInput, DocInput, Fields, NG_FIELDS, PeopleFields, PeopleKind, PersonInput, StampError,
    TEXT_FIELDS, check_stamp, corr_doc, people_schema, people_term, person_doc, register, schema,
    stamp, to_tantivy,
};

/// Normalisation version these rules implement (store it with the index); must equal every vector file's `norm_version`.
/// Bumped with any change that makes a query term differ from the terms an existing index holds.
pub const NORM_VERSION: u32 = 1;
/// Index layout version written to each index directory's `VERSION` file (2: documents keyed by
/// the caller's stable message id, hpfx, collapse fields).
pub const LAYOUT_VERSION: u32 = 2;

/// The index directory's version stamp file (`idx-N/VERSION`).
pub const VERSION_FILE: &str = "VERSION";
/// Sub-directories of `idx-N`: the message index and the people index.
pub const MAIL_DIR: &str = "mail";
pub const PEOPLE_DIR: &str = "people";
/// `$data/search/CURRENT` names the active `idx-N`.
pub const CURRENT_FILE: &str = "CURRENT";

/// The linked Tantivy crate version ("0.27.0").
pub fn tantivy_version() -> &'static str {
    let s = tantivy::version_string();
    s.split(',')
        .next()
        .unwrap_or(s)
        .trim()
        .trim_start_matches("tantivy v")
}

/// FNV-1a 64 (`from_h`, `msgid_h`, `body_h`, own-address hashes; the C# side uses the same).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Name of index directory number `n` under the search root.
pub fn idx_name(n: u32) -> String {
    format!("idx-{n}")
}

/// The active directory number from `root/CURRENT` (`idx-N`, optional trailing newline).
pub fn read_current(root: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(root.join(CURRENT_FILE))
        .ok()?
        .trim()
        .strip_prefix("idx-")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn fnv_and_versions() {
        assert_eq!(super::fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(super::fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(super::tantivy_version(), "0.27.0");
        assert_eq!(super::idx_name(3), "idx-3");
    }
}

//! Index layout.
//!
//! Message index (`idx-N/mail`): one document per message, keyed by the caller's stable message id
//! (`mid`: indexed, so upserts and deletes are by term; fast, so results name the message).
//! `skey` (`search_key` = `received_at·2²⁰ + slot`) is immutable, indexed (date ranges, paging) and
//! fast (date order, recency, zone maps), never the identity. `from_h`, `account`, `bulk` feed
//! ranking and scope; `conv` (indexed + fast) the Top-hits quoted fallback and collapse; `dup_mid`
//! and `dup_body` the result-time duplicate collapse. Text fields use the `mail` word analyzer
//! after N1, unsegmented-script runs go only to `ng_*` (`cjk2` bigrams), and `hpfx` holds the
//! 1–3-letter prefixes of subject/from/to/cc words for the typing path. No stored fields.
//!
//! People index (`idx-N/people`): one `Person` document (names + address parts, address hash) and
//! one `Corr` document (affinity counts) per `correspondents.id`, both deleted by [`people_term`].
//!
//! Every index directory carries a `VERSION` stamp ([`stamp`]); the reader and the indexer refuse an
//! index whose norm version, layout version, Tantivy version or index format differs
//! ([`check_stamp`], fail closed).

use crate::norm::{link_hosts_into, n1_cow, number_groups};
use crate::text::{CjkBigram, edge_prefixes, has_cjk, mail_analyzer, strip_cjk};
use crate::{LAYOUT_VERSION, NORM_VERSION, VERSION_FILE, fnv1a64, tantivy_version};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, TantivyDocument, Term};

/// Word fields, in [`Fields::text`] order.
pub const TEXT_FIELDS: [&str; 11] = [
    "subject",
    "from_",
    "to_",
    "cc_",
    "bcc_",
    "body",
    "quoted",
    "att_names",
    "att_text",
    "links",
    "numbers",
];
/// Bigram fields, in [`Fields::ng`] order.
pub const NG_FIELDS: [&str; 5] = ["ng_subject", "ng_people", "ng_body", "ng_quoted", "ng_att"];
/// Word fields without BM25 length norms (sparse fields).
pub const NO_NORMS: [&str; 4] = ["att_names", "att_text", "links", "numbers"];
/// The short-prefix field: 1..=`HPFX_MAX` character prefixes of subject/from/to/cc words.
pub const HPFX: &str = "hpfx";
pub const HPFX_MAX: usize = 3;

fn text_opts(tokenizer: &str, rec: IndexRecordOption, norms: bool) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(rec)
            .set_fieldnorms(norms),
    )
}

/// The message index schema.
pub fn schema() -> Schema {
    let mut b = Schema::builder();
    b.add_u64_field("mid", INDEXED | FAST);
    b.add_u64_field("skey", INDEXED | FAST);
    b.add_u64_field("from_h", INDEXED | FAST);
    b.add_u64_field("account", INDEXED | FAST);
    b.add_u64_field("bulk", FAST);
    b.add_u64_field("conv", INDEXED | FAST);
    b.add_u64_field("dup_mid", FAST);
    b.add_u64_field("dup_body", FAST);
    for f in TEXT_FIELDS {
        b.add_text_field(
            f,
            text_opts(
                "mail",
                IndexRecordOption::WithFreqsAndPositions,
                !NO_NORMS.contains(&f),
            ),
        );
    }
    for f in NG_FIELDS {
        b.add_text_field(
            f,
            text_opts("cjk2", IndexRecordOption::WithFreqsAndPositions, true),
        );
    }
    b.add_text_field(HPFX, text_opts("mail", IndexRecordOption::Basic, false));
    b.build()
}

/// Registers the `mail` and `cjk2` tokenizers (message and people index, writer and reader).
pub fn register(index: &Index) {
    let t = index.tokenizers();
    t.register("mail", mail_analyzer());
    t.register("cjk2", TextAnalyzer::builder(CjkBigram).build());
}

/// Field handles of the message index.
#[derive(Clone, Debug)]
pub struct Fields {
    pub mid: Field,
    pub skey: Field,
    pub from_h: Field,
    pub account: Field,
    pub bulk: Field,
    pub conv: Field,
    pub dup_mid: Field,
    pub dup_body: Field,
    /// [`TEXT_FIELDS`] order
    pub text: [(Field, &'static str); 11],
    /// [`NG_FIELDS`] order
    pub ng: [(Field, &'static str); 5],
    pub hpfx: Field,
}

impl Fields {
    /// Handles of `sc`; `Err` names the first missing field (not this layout).
    pub fn new(sc: &Schema) -> tantivy::Result<Fields> {
        let f = |n: &str| sc.get_field(n);
        let mut text = [(Field::from_field_id(0), ""); 11];
        for (i, n) in TEXT_FIELDS.iter().enumerate() {
            text[i] = (f(n)?, *n);
        }
        let mut ng = [(Field::from_field_id(0), ""); 5];
        for (i, n) in NG_FIELDS.iter().enumerate() {
            ng[i] = (f(n)?, *n);
        }
        Ok(Fields {
            mid: f("mid")?,
            skey: f("skey")?,
            from_h: f("from_h")?,
            account: f("account")?,
            bulk: f("bulk")?,
            conv: f("conv")?,
            dup_mid: f("dup_mid")?,
            dup_body: f("dup_body")?,
            text,
            ng,
            hpfx: f(HPFX)?,
        })
    }

    /// The handle of word field `name` ([`TEXT_FIELDS`]).
    pub fn text_field(&self, name: &str) -> Option<Field> {
        self.text.iter().find(|x| x.1 == name).map(|x| x.0)
    }
}

/// One message as the writer receives it. A
/// `Doc` with `cont` appends its text fields to the previous `DocInput` of the same mid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocInput {
    /// The caller's stable message id.
    pub mid: u64,
    /// messages.search_key (immutable)
    pub search_key: i64,
    /// [`fnv1a64`] of the lowercased sender address
    pub from_h: u64,
    pub account: u64,
    pub bulk: bool,
    pub subject: String,
    /// display name + address of the sender
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub body: String,
    pub quoted: String,
    pub att_names: String,
    pub att_text: String,
    /// link URLs (hosts are derived here, together with the links in `body`)
    pub links: String,
    /// conversation id, 0 = own conversation
    pub conv: u64,
    /// hash of the normalised Message-ID, 0 = none
    pub msgid_h: u64,
    /// hash of the normalised body text, 0 = none
    pub body_h: u64,
}

/// The message document: N1 on every text, CJK runs blanked in the word fields and written to the
/// `ng_*` fields, link hosts (from `links` and `body`), digit groups (subject + body) and the
/// `hpfx` edge prefixes derived here.
pub fn to_tantivy(f: &Fields, d: &DocInput) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_u64(f.mid, d.mid);
    doc.add_u64(f.skey, d.search_key.max(0) as u64);
    doc.add_u64(f.from_h, d.from_h);
    doc.add_u64(f.account, d.account);
    doc.add_u64(f.bulk, d.bulk as u64);
    doc.add_u64(f.conv, d.conv);
    doc.add_u64(f.dup_mid, d.msgid_h);
    doc.add_u64(f.dup_body, d.body_h);
    let subject = n1_cow(&d.subject);
    let from = n1_cow(&d.from);
    let to = n1_cow(&d.to);
    let cc = n1_cow(&d.cc);
    let bcc = n1_cow(&d.bcc);
    let body = n1_cow(&d.body);
    let quoted = n1_cow(&d.quoted);
    let att = n1_cow(&d.att_names);
    let att_text = n1_cow(&d.att_text);
    let words: [&str; 9] = [
        &subject, &from, &to, &cc, &bcc, &body, &quoted, &att, &att_text,
    ];
    for (i, v) in words.iter().enumerate() {
        if !v.is_empty() {
            doc.add_text(f.text[i].0, strip_cjk(v));
        }
    }
    let mut links = String::new();
    link_hosts_into(&mut links, &n1_cow(&d.links));
    link_hosts_into(&mut links, &body);
    if !links.is_empty() {
        doc.add_text(f.text[9].0, &links);
    }
    let nums = number_groups(&format!("{subject}\n{body}"));
    if !nums.is_empty() {
        doc.add_text(f.text[10].0, &nums);
    }
    let people = format!("{from} {to} {cc} {bcc}");
    let att_all = format!("{att} {att_text}");
    let ng: [&str; 5] = [&subject, &people, &body, &quoted, &att_all];
    for (i, v) in ng.iter().enumerate() {
        if has_cjk(v) {
            doc.add_text(f.ng[i].0, v);
        }
    }
    let p = edge_prefixes(&mut mail_analyzer(), &[&subject, &from, &to, &cc], HPFX_MAX);
    if !p.is_empty() {
        doc.add_text(f.hpfx, &p);
    }
    doc
}

// ---- people index -------------------------------------------------------------------------------

/// The two document kinds of the people index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeopleKind {
    Person = 0,
    Corr = 1,
}

/// The people index schema: `pkey` (= id·2 + kind, the delete term), `kind`, `id`
/// (correspondents.id), `account`, `addr_h` ([`fnv1a64`] of `addr_norm`), `names` (display names
/// and address parts, `mail` analyzer) on Person documents; the affinity counts on Corr documents.
pub fn people_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_u64_field("pkey", INDEXED);
    b.add_u64_field("kind", INDEXED | FAST);
    b.add_u64_field("id", FAST);
    b.add_u64_field("account", FAST);
    b.add_u64_field("addr_h", FAST);
    for f in [
        "sent_to",
        "received_from",
        "replied_to",
        "cc_with",
        "last_at",
        "is_contact",
    ] {
        b.add_u64_field(f, FAST);
    }
    b.add_text_field(
        "names",
        text_opts("mail", IndexRecordOption::WithFreqs, true),
    );
    b.build()
}

#[derive(Clone, Debug)]
pub struct PeopleFields {
    pub pkey: Field,
    pub kind: Field,
    pub id: Field,
    pub account: Field,
    pub addr_h: Field,
    pub sent_to: Field,
    pub received_from: Field,
    pub replied_to: Field,
    pub cc_with: Field,
    pub last_at: Field,
    pub is_contact: Field,
    pub names: Field,
}

impl PeopleFields {
    pub fn new(sc: &Schema) -> tantivy::Result<PeopleFields> {
        let f = |n: &str| sc.get_field(n);
        Ok(PeopleFields {
            pkey: f("pkey")?,
            kind: f("kind")?,
            id: f("id")?,
            account: f("account")?,
            addr_h: f("addr_h")?,
            sent_to: f("sent_to")?,
            received_from: f("received_from")?,
            replied_to: f("replied_to")?,
            cc_with: f("cc_with")?,
            last_at: f("last_at")?,
            is_contact: f("is_contact")?,
            names: f("names")?,
        })
    }
}

/// One person (people index).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersonInput {
    /// correspondents.id
    pub id: u64,
    pub account: u64,
    pub addr_norm: String,
    pub names: Vec<String>,
}

/// One correspondent (people index).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CorrInput {
    /// correspondents.id
    pub id: u64,
    pub sent_to: u32,
    pub received_from: u32,
    pub replied_to: u32,
    pub cc_with: u32,
    pub first_at: i64,
    pub last_at: i64,
    pub is_contact: bool,
}

/// The delete term of one people document (upsert = delete this term + add).
pub fn people_term(f: &PeopleFields, id: u64, kind: PeopleKind) -> Term {
    Term::from_field_u64(f.pkey, id.wrapping_mul(2) | kind as u64)
}

pub fn person_doc(f: &PeopleFields, p: &PersonInput) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_u64(f.pkey, p.id.wrapping_mul(2));
    doc.add_u64(f.kind, PeopleKind::Person as u64);
    doc.add_u64(f.id, p.id);
    doc.add_u64(f.account, p.account);
    doc.add_u64(f.addr_h, fnv1a64(p.addr_norm.to_lowercase().as_bytes()));
    let mut names = String::new();
    for n in p.names.iter().chain(std::iter::once(&p.addr_norm)) {
        if !names.is_empty() {
            names.push('\n');
        }
        names.push_str(&strip_cjk(&n1_cow(n)));
    }
    doc.add_text(f.names, &names);
    doc
}

pub fn corr_doc(f: &PeopleFields, c: &CorrInput) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_u64(f.pkey, c.id.wrapping_mul(2) | 1);
    doc.add_u64(f.kind, PeopleKind::Corr as u64);
    doc.add_u64(f.id, c.id);
    doc.add_u64(f.sent_to, c.sent_to as u64);
    doc.add_u64(f.received_from, c.received_from as u64);
    doc.add_u64(f.replied_to, c.replied_to as u64);
    doc.add_u64(f.cc_with, c.cc_with as u64);
    doc.add_u64(f.last_at, c.last_at.max(0) as u64);
    doc.add_u64(f.is_contact, c.is_contact as u64);
    doc
}

// ---- VERSION stamp ------------------------------------------------------------------------------

/// The `VERSION` file content of an index directory written by this build.
pub fn stamp() -> String {
    format!(
        "{{\"norm_version\":{NORM_VERSION},\"layout_version\":{LAYOUT_VERSION},\"tantivy\":\"{}\",\"index_format\":{}}}\n",
        tantivy_version(),
        tantivy::INDEX_FORMAT_VERSION
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StampError {
    Missing,
    Malformed,
    /// (component, found, expected)
    Mismatch(&'static str, String, String),
}

impl std::fmt::Display for StampError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            StampError::Missing => write!(f, "VERSION missing"),
            StampError::Malformed => write!(f, "VERSION malformed"),
            StampError::Mismatch(c, a, b) => write!(f, "VERSION {c} {a} != {b}"),
        }
    }
}

impl std::error::Error for StampError {}

fn stamp_field<'a>(s: &'a str, k: &str) -> Option<&'a str> {
    let i = s.find(&format!("\"{k}\":"))? + k.len() + 3;
    let rest = &s[i..];
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find(['"', ',', '}'])?;
    Some(&rest[..end])
}

/// Fail-closed check of `dir/VERSION` (an `idx-N` directory) against this build.
pub fn check_stamp(dir: &std::path::Path) -> Result<(), StampError> {
    let s = match std::fs::read(dir.join(VERSION_FILE)) {
        Ok(b) => String::from_utf8(b).map_err(|_| StampError::Malformed)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StampError::Missing),
        Err(_) => return Err(StampError::Malformed),
    };
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err(StampError::Malformed);
    }
    let want = [
        ("norm_version", NORM_VERSION.to_string()),
        ("layout_version", LAYOUT_VERSION.to_string()),
        ("tantivy", tantivy_version().to_string()),
        ("index_format", tantivy::INDEX_FORMAT_VERSION.to_string()),
    ];
    for (k, v) in want {
        let got = stamp_field(s, k).ok_or(StampError::Malformed)?;
        if got != v {
            return Err(StampError::Mismatch(k, got.to_string(), v));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_roundtrip_and_refusals() {
        let d = std::env::temp_dir().join(format!("mailsearch-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(check_stamp(&d), Err(StampError::Missing));
        std::fs::write(d.join(VERSION_FILE), stamp()).unwrap();
        assert_eq!(check_stamp(&d), Ok(()));
        std::fs::write(d.join(VERSION_FILE), "garbage").unwrap();
        assert_eq!(check_stamp(&d), Err(StampError::Malformed));
        std::fs::write(
            d.join(VERSION_FILE),
            stamp().replace("\"norm_version\":1", "\"norm_version\":9"),
        )
        .unwrap();
        assert!(matches!(
            check_stamp(&d),
            Err(StampError::Mismatch("norm_version", _, _))
        ));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn document_derivations() {
        let sc = schema();
        let f = Fields::new(&sc).unwrap();
        let d = DocInput {
            mid: 7,
            search_key: 1 << 40,
            subject: "Invoice 会議 555-123-4567".into(),
            from: "Ann <ann@x.org>".into(),
            body: "see https://www.Example.com/a".into(),
            ..Default::default()
        };
        let doc = to_tantivy(&f, &d);
        let texts = |fl: Field| -> Vec<String> {
            doc.get_all(fl)
                .filter_map(|v| tantivy::schema::Value::as_str(&v).map(str::to_string))
                .collect()
        };
        assert_eq!(texts(f.text[9].0), vec!["www.example.com"]);
        assert_eq!(texts(f.text[10].0), vec!["5551234567"]);
        assert_eq!(texts(f.ng[0].0), vec!["Invoice 会議 555-123-4567"]);
        assert!(!texts(f.text[0].0)[0].contains('会'));
        let h = texts(f.hpfx);
        assert!(h[0].split(' ').any(|p| p == "inv") && h[0].split(' ').any(|p| p == "an"));
    }
}

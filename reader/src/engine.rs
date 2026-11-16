//! The search engine over the read-only index (what `libmailsearch` exposes): message id keying,
//! excluded set in the collector, duplicate collapse, VERSION fail-closed, lease, typing path,
//! quoted-text fallback, relevance list without recency, release.
//!
//! # Public Rust API (used by the C ABI in `lib.rs` and by the evaluation bench)
//!
//! - [`Engine::open`]`(search_root) -> Result<Engine, OpenError>`: `CURRENT` → `idx-N`, `VERSION`
//!   check (fail closed: [`OpenError::Version`]), `mail/` and (if present) `people/` read-only.
//! - [`Engine::search`]`(&Query, &Opts) -> Result<Hits, SearchError>`: one query. [`Query`] mirrors
//!   `MailsearchQuery` (clauses, dates, account, typing / skip / relevance / collapse / exclusion flags,
//!   `n_top`, `n_list`); [`Opts`] holds the ranking profile and tunables ([`Opts::default`] = the
//!   defaults: profile 1, recency `feat.wr` 0.1 / 365 d in Top hits only — set `feat.wr = 0` for no
//!   recency —, prefix 1, stems 0.15, typo 0.1, `k_rank` 20,000, count exact to 10,000,
//!   [`Qfb::Conv`] fallback). [`Hits`] has `top` (Top hits, with recency and the quoted fallback),
//!   `date` (newest first), `rel` (relevance order: no recency, quoted 0.25; filled when
//!   `Query::relevance` or `Opts::rel_list`), `count`, `count_capped`, `from_recent`, `corrected`,
//!   `labels` (tier label per Top hit: 0 exact/prefix, 1 stem only, 2 typo only), `folded` (the
//!   copies collapse folded into each row, with `Opts::folded`) and `info` (timings and counters).
//! - [`Engine::oracle`]`(&Query, &Opts, &HashSet<u64> targets) -> Oracle`: a per-query
//!   evidence record over the complete matched set ([`Oracle::to_json`] prints it as one JSON line).
//! - [`Engine::set_excluded`], [`Engine::set_owner`], [`Engine::set_languages`],
//!   [`Engine::set_affinity_accounts`] (evaluation: affinity from these accounts' people only),
//!   [`Engine::people`] (suggestions), [`Engine::highlight`], [`Engine::reload`] (→ lease `(dir,
//!   gen)`, then [`Engine::release`]), [`Engine::release`], [`Engine::num_docs`],
//!   [`Engine::cancel_flag`] (an `Arc<AtomicBool>` checked between segments and every 4,096
//!   documents; cleared when the next search starts).
//!
//! Notes: "at most 3 typo candidates per term" is not enforced: the measured engine
//! expands the typo tier with Tantivy's `FuzzyTermQuery` (distance 1, transpositions) as is. Hit
//! `tiers`/`fields` are filled on the pause/Enter query only (0 on the typing path, which keeps its
//! keystroke budget); a match through `hpfx` sets the prefix tier but no field bit.

use crate::dir::RoDir;
use mailsearch_core::layout::HPFX_MAX;
use mailsearch_core::norm::n1;
use mailsearch_core::stem::{Snowball, eligible};
use mailsearch_core::text::{CompiledWord, QueryToken, compile_word, mail_analyzer};
use mailsearch_core::{
    Fields, MAIL_DIR, PEOPLE_DIR, PeopleFields, StampError, check_stamp, fnv1a64, idx_name,
    read_current,
};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tantivy::columnar::ColumnValues;
use tantivy::index::SegmentId;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, EnableScoring, FuzzyTermQuery, Occur,
    PhrasePrefixQuery, PhraseQuery, Query as TQuery, RangeQuery, Scorer, TermQuery, TermSetQuery,
    Weight,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{
    BitSet, DocAddress, DocId, DocSet, Index, IndexReader, ReloadPolicy, Searcher, TERMINATED, Term,
};

/// Zone-map block size (documents) of the early-termination walk.
pub const ZONE: u32 = 512;
/// Cancellation is checked every this many documents a scorer visits (and between segments).
pub const CANCEL_EVERY: u32 = 4096;

// MAILSEARCH_F_* (clause fields) and MAILSEARCH_T_* (tiers), as in mailsearch.h.
pub const F_SUBJECT: u32 = 1 << 0;
pub const F_FROM: u32 = 1 << 1;
pub const F_TO: u32 = 1 << 2;
pub const F_CC: u32 = 1 << 3;
pub const F_BCC: u32 = 1 << 4;
pub const F_BODY: u32 = 1 << 5;
pub const F_QUOTED: u32 = 1 << 6;
pub const F_ATT_NAME: u32 = 1 << 7;
pub const F_ATT_TEXT: u32 = 1 << 8;
pub const F_LINKS: u32 = 1 << 9;
pub const F_NUMBERS: u32 = 1 << 10;
pub const F_TEXT: u32 = (1 << 11) - 1;
pub const T_EXACT: u32 = 1 << 0;
pub const T_PREFIX: u32 = 1 << 1;
pub const T_STEM: u32 = 1 << 2;
pub const T_TYPO: u32 = 1 << 3;

/// Word field of each MAILSEARCH_F_* bit (bit i ↔ `TEXT_FIELDS[i]`), and its bigram field.
const FIELD_BITS: [(u32, &str, &str); 11] = [
    (F_SUBJECT, "subject", "ng_subject"),
    (F_FROM, "from_", "ng_people"),
    (F_TO, "to_", "ng_people"),
    (F_CC, "cc_", "ng_people"),
    (F_BCC, "bcc_", "ng_people"),
    (F_BODY, "body", "ng_body"),
    (F_QUOTED, "quoted", "ng_quoted"),
    (F_ATT_NAME, "att_names", "ng_att"),
    (F_ATT_TEXT, "att_text", "ng_att"),
    (F_LINKS, "links", ""),
    (F_NUMBERS, "numbers", ""),
];

// ---- public types -------------------------------------------------------------------------------

/// Clause match mode (`MAILSEARCH_M_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// exact + prefix (≥ 2 characters) + stem and typo tiers (pause/Enter only)
    #[default]
    Default,
    /// `+word`: exact token only
    Exact,
    /// `"a b"`: exact tokens in order
    Phrase,
    /// `word*`: explicit prefix
    Prefix,
    /// `a AROUND n b`: the clause's word tokens at most n tokens apart, in either order (exact tokens)
    Near(u32),
}

/// One clause of the caller's query compiler (`MailsearchClause`).
#[derive(Clone, Debug, Default)]
pub struct Clause {
    /// raw user text of one term or phrase (normalised and tokenised here)
    pub text: String,
    /// MAILSEARCH_F_* bitmask
    pub fields: u32,
    pub mode: Mode,
    /// the word is still being typed
    pub partial: bool,
    /// NOT (exact tokens; an explicit prefix stays a prefix)
    pub negate: bool,
    /// 0: AND-ed on its own; equal non-zero groups are OR-ed and the group AND-ed (a negated
    /// clause in a group is the alternative "NOT x")
    pub group: u32,
}

/// One query (`MailsearchQuery`).
#[derive(Clone, Debug)]
pub struct Query {
    pub clauses: Vec<Clause>,
    /// unix seconds (recency)
    pub now: i64,
    /// DateRange filter on received time, [lo, hi) unix seconds
    pub date: Option<(i64, i64)>,
    /// DateHint (feature D), [lo, hi) unix seconds
    pub hint: Option<(i64, i64)>,
    /// date-list paging: only search_key < before_key (0 = from the newest); the count, Top hits
    /// and relevance list are unaffected
    pub before_key: i64,
    /// 0 = every account
    pub account: u64,
    /// typing path (hpfx, no stems/typo, early termination, two passes)
    pub typing: bool,
    /// date list and count only
    pub skip_top: bool,
    /// Top hits only (with `typing`, the count is then a lower bound: use pass 1's)
    pub skip_list: bool,
    /// the list wanted is in relevance order (fills `Hits::rel`)
    pub relevance: bool,
    /// result-time duplicate collapse
    pub collapse: bool,
    /// apply the excluded-key set (false = in:anywhere)
    pub exclude: bool,
    /// Top hits wanted
    pub n_top: usize,
    /// list rows wanted
    pub n_list: usize,
}

impl Default for Query {
    fn default() -> Query {
        Query {
            clauses: Vec::new(),
            now: 0,
            date: None,
            hint: None,
            before_key: 0,
            account: 0,
            typing: false,
            skip_top: false,
            skip_list: false,
            relevance: false,
            collapse: true,
            exclude: true,
            n_top: 5,
            n_list: 50,
        }
    }
}

/// Column weights of the text score B.
#[derive(Clone, Copy, Debug)]
pub struct ColW {
    pub subject: f32,
    pub from: f32,
    pub to: f32,
    pub cc: f32,
    pub bcc: f32,
    pub body: f32,
    pub quoted: f32,
    pub att_names: f32,
    pub att_text: f32,
    pub links: f32,
    pub numbers: f32,
}

impl ColW {
    /// Ranking profile 1.
    pub fn profile1() -> ColW {
        ColW {
            subject: 1.5,
            from: 2.5,
            to: 1.5,
            cc: 1.0,
            bcc: 1.0,
            body: 1.0,
            quoted: 0.25,
            att_names: 4.0,
            att_text: 0.6,
            links: 0.3,
            numbers: 1.0,
        }
    }
    fn of(&self, name: &str) -> f32 {
        match name {
            "subject" | "ng_subject" | "hpfx" => self.subject,
            "from_" | "ng_people" => self.from,
            "to_" => self.to,
            "cc_" => self.cc,
            "bcc_" => self.bcc,
            "body" | "ng_body" => self.body,
            "quoted" | "ng_quoted" => self.quoted,
            "att_names" | "ng_att" => self.att_names,
            "att_text" => self.att_text,
            "links" => self.links,
            _ => self.numbers,
        }
    }
}

/// Feature weights of the ranking score.
#[derive(Clone, Copy, Debug)]
pub struct FeatW {
    pub wt: f64,
    /// recency, Top hits only (0 = off)
    pub wr: f64,
    pub half_life_days: f64,
    pub wa: f64,
    pub wo: f64,
    /// DateHint (feature D)
    pub wd: f64,
    pub wb: f64,
    /// people-first bonus (0 in profile 1)
    pub wp: f64,
}

/// When a term's quoted-text match counts at `qfb_w` in Top hits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Qfb {
    Off,
    /// no match for the term outside quoted text in the message, nor in any message of its
    /// conversation in the query's scope
    Conv,
}

/// Ranking profile and tunables.
#[derive(Clone, Debug)]
pub struct Opts {
    pub colw: ColW,
    pub feat: FeatW,
    pub prefix_w: f32,
    pub stem_w: f32,
    pub typo_w: f32,
    /// query-time Snowball stems (≤ 8 per word; pause/Enter only)
    pub stems: bool,
    /// typo tier (pause/Enter only)
    pub typo: bool,
    /// people-first resolution (fuzzy person matches add candidates)
    pub people_first: bool,
    /// Top hits candidate cap K_rank
    pub k_rank: usize,
    /// the count is exact up to this many (collapsed) matches
    pub count_cap: usize,
    /// typing path: partial words of 2..=`pfx_short` letters use `hpfx` + the exact word
    pub pfx_short: usize,
    pub qfb: Qfb,
    /// the quoted weight when the fallback applies (the body weight)
    pub qfb_w: f32,
    /// also fill `Hits::rel` when the query asks for the date list (bench)
    pub rel_list: bool,
    /// fill `Hits::folded` (evaluation: a copy folded into a row counts as found)
    pub folded: bool,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            colw: ColW::profile1(),
            feat: FeatW {
                wt: 1.0,
                wr: 0.1,
                half_life_days: 365.0,
                wa: 0.5,
                wo: 0.2,
                wd: 0.3,
                wb: 0.0,
                wp: 0.0,
            },
            prefix_w: 1.0,
            stem_w: 0.15,
            typo_w: 0.1,
            stems: true,
            typo: true,
            people_first: true,
            k_rank: 20_000,
            count_cap: 10_000,
            pfx_short: HPFX_MAX,
            qfb: Qfb::Conv,
            qfb_w: 1.0,
            rel_list: false,
            folded: false,
        }
    }
}

/// One result row (`MailsearchHit`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Hit {
    pub mid: u64,
    pub skey: u64,
    /// final score (0 in date order)
    pub score: f32,
    /// T_* that matched
    pub tiers: u32,
    /// F_* that matched
    pub fields: u32,
    /// collapsed rows folded into this one
    pub copies: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Hits {
    pub top: Vec<Hit>,
    pub date: Vec<Hit>,
    pub rel: Vec<Hit>,
    /// matches after exclusion and collapse, exact up to `Opts::count_cap`
    pub count: u64,
    pub count_capped: bool,
    /// candidate cap applied: Top hits ranked from recent matches
    pub from_recent: bool,
    /// the typo tier contributed a result
    pub corrected: bool,
    /// per Top hit: 0 exact/prefix, 1 stem only, 2 typo only
    pub labels: Vec<u8>,
    /// with `Opts::folded`: the copies collapse folded into each row
    pub folded: Folded,
    pub info: Vec<(&'static str, f64)>,
}

/// Copies result-time collapse folded into a row, per list, as (row index, the copy's `skey`).
#[derive(Clone, Debug, Default)]
pub struct Folded {
    pub top: Vec<(u32, u64)>,
    pub date: Vec<(u32, u64)>,
    pub rel: Vec<(u32, u64)>,
}

#[derive(Debug)]
pub enum OpenError {
    /// the index is missing or unreadable
    Io(String),
    /// VERSION missing, malformed or different (fail closed)
    Version(StampError),
    /// a segment file failed its integrity check (`mailsearch_core::integrity`)
    Corrupt(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            OpenError::Io(e) => write!(f, "index: {e}"),
            OpenError::Version(e) => write!(f, "index version: {e}"),
            OpenError::Corrupt(e) => write!(f, "index corrupt: {e}"),
        }
    }
}

impl From<tantivy::TantivyError> for OpenError {
    fn from(e: tantivy::TantivyError) -> Self {
        OpenError::Io(e.to_string())
    }
}

#[derive(Debug)]
pub enum SearchError {
    Cancelled,
    Io(String),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SearchError::Cancelled => write!(f, "cancelled"),
            SearchError::Io(e) => write!(f, "index: {e}"),
        }
    }
}

impl From<tantivy::TantivyError> for SearchError {
    fn from(e: tantivy::TantivyError) -> Self {
        SearchError::Io(e.to_string())
    }
}

type SResult<T> = Result<T, SearchError>;
/// Boolean query clauses.
type Subs = Vec<(Occur, Box<dyn TQuery>)>;

// ---- collector ---------------------------------------------------------------------------------

/// A set of u64 keys with a cheap multiplicative hash (excluded message id set, collapse keys).
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);
impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for x in b {
            self.0 = (self.0 ^ *x as u64).wrapping_mul(0x100000001b3);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (v ^ (v >> 29)).wrapping_mul(0xbf58476d1ce4e5b9);
    }
}
pub type KeySet = HashSet<u64, std::hash::BuildHasherDefault<IdHasher>>;

#[derive(Clone, Copy, Debug)]
struct Cand {
    skey: u64,
    mid: u64,
    score: f32,
    from_h: u64,
    bulk: bool,
    conv: u64,
    /// normalised Message-ID hash, 0 = none
    dup_mid: u64,
    /// normalised body hash, 0 = none
    dup_body: u64,
    addr: DocAddress,
}

/// Greedy duplicate detection: a message is a copy of an earlier
/// one when either its Message-ID hash or its body hash matches (two fields, not a cascade).
#[derive(Default)]
struct DupSeen {
    mid: KeySet,
    body: KeySet,
}

impl DupSeen {
    /// Is this message new? Records its keys either way.
    fn insert(&mut self, dup_mid: u64, dup_body: u64) -> bool {
        let seen = (dup_mid != 0 && self.mid.contains(&dup_mid))
            || (dup_body != 0 && self.body.contains(&dup_body));
        if dup_mid != 0 {
            self.mid.insert(dup_mid);
        }
        if dup_body != 0 {
            self.body.insert(dup_body);
        }
        !seen
    }
}

impl PartialEq for Cand {
    fn eq(&self, o: &Self) -> bool {
        self.skey == o.skey && self.mid == o.mid
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    // reversed: BinaryHeap pops the oldest (smallest search_key) first
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        o.skey.cmp(&self.skey).then(o.mid.cmp(&self.mid))
    }
}

/// One collection pass: count (and distinct duplicate keys), the newest `cap` by search_key, every
/// match from `extra` senders (top-50 correspondents), scores when `scoring`; excluded messages are
/// skipped before they are counted. `early`: segments newest-first, ZONE blocks and whole segments
/// skipped once `cap` newer hits are held and `count_need` matches are counted.
struct Coll {
    cap: usize,
    extra: Option<Arc<HashSet<u64>>>,
    scoring: bool,
    bits: Option<Arc<HashMap<SegmentId, Arc<BitSet>>>>,
    collapse: bool,
    /// distinct keys are tracked up to this many (count cap + 1)
    key_cap: usize,
    early: bool,
    count_need: usize,
}

#[derive(Default)]
struct Acc {
    heap: BinaryHeap<Cand>,
    ex: Vec<Cand>,
    count: usize,
    distinct: usize,
    keys: DupSeen,
}

impl Acc {
    fn counted(&self, collapse: bool) -> usize {
        if collapse { self.distinct } else { self.count }
    }
}

#[derive(Default)]
struct Fruit {
    count: usize,
    /// distinct duplicate keys (= count without collapse), up to the count cap + 1
    distinct: usize,
    /// newest first
    newest: Vec<Cand>,
    extra: Vec<Cand>,
    /// the early walk skipped segments or blocks
    skipped: bool,
}

struct SegCols {
    skey: Arc<dyn ColumnValues<u64>>,
    mid: Arc<dyn ColumnValues<u64>>,
    from: Arc<dyn ColumnValues<u64>>,
    bulk: Arc<dyn ColumnValues<u64>>,
    conv: Arc<dyn ColumnValues<u64>>,
    dup_mid: Arc<dyn ColumnValues<u64>>,
    dup_body: Arc<dyn ColumnValues<u64>>,
}

fn col(seg: &tantivy::SegmentReader, name: &str) -> tantivy::Result<Arc<dyn ColumnValues<u64>>> {
    Ok(seg.fast_fields().u64(name)?.first_or_default_col(0))
}

impl SegCols {
    fn new(seg: &tantivy::SegmentReader) -> tantivy::Result<SegCols> {
        Ok(SegCols {
            skey: col(seg, "skey")?,
            mid: col(seg, "mid")?,
            from: col(seg, "from_h")?,
            bulk: col(seg, "bulk")?,
            conv: col(seg, "conv")?,
            dup_mid: col(seg, "dup_mid")?,
            dup_body: col(seg, "dup_body")?,
        })
    }
}

impl Coll {
    #[inline]
    fn can_skip(&self, acc: &Acc, max: u64) -> bool {
        self.cap > 0
            && acc.heap.len() >= self.cap
            && acc.counted(self.collapse) >= self.count_need
            && acc.heap.peek().is_some_and(|m| max < m.skey)
    }

    #[inline]
    fn collect(&self, acc: &mut Acc, c: &SegCols, ord: u32, doc: DocId, score: f32) {
        acc.count += 1;
        let mid = c.mid.get_val(doc);
        let (dup_mid, dup_body) = (c.dup_mid.get_val(doc), c.dup_body.get_val(doc));
        if self.collapse && acc.distinct < self.key_cap && acc.keys.insert(dup_mid, dup_body) {
            acc.distinct += 1;
        }
        let skey = c.skey.get_val(doc);
        let mk = || Cand {
            skey,
            mid,
            score,
            from_h: c.from.get_val(doc),
            bulk: c.bulk.get_val(doc) != 0,
            conv: c.conv.get_val(doc),
            dup_mid,
            dup_body,
            addr: DocAddress::new(ord, doc),
        };
        if let Some(x) = &self.extra
            && x.contains(&c.from.get_val(doc))
        {
            acc.ex.push(mk());
        }
        if acc.heap.len() < self.cap {
            acc.heap.push(mk());
        } else if acc
            .heap
            .peek()
            .is_some_and(|m| (skey, mid) > (m.skey, m.mid))
        {
            acc.heap.pop();
            acc.heap.push(mk());
        }
    }
}

// ---- query terms --------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum TK {
    Tiers {
        t: String,
        prefix: bool,
        /// typing path short prefix: `hpfx` + the exact word in every field
        short: bool,
        /// Default-mode positive word: stem and typo tiers may apply (pause/Enter only)
        expand: bool,
        stems: Vec<String>,
        typo: bool,
    },
    Phrase {
        parts: Vec<String>,
        last_prefix: bool,
    },
    /// `Mode::Near`: the parts at most `slop` tokens apart, in either order
    Near {
        parts: Vec<String>,
        slop: u32,
    },
    Cjk {
        bigrams: Vec<String>,
    },
}

#[derive(Clone, Debug)]
struct T {
    fields: u32,
    tk: TK,
    partial: bool,
}

#[derive(Clone, Debug)]
struct CClause {
    /// AND-ed parts of one clause (CJK runs and the word part)
    terms: Vec<T>,
    negate: bool,
    group: u32,
}

#[derive(Clone, Copy)]
struct Tiers {
    exact: bool,
    prefix: bool,
    stem: bool,
    typo: bool,
}
const ALL: Tiers = Tiers {
    exact: true,
    prefix: true,
    stem: true,
    typo: true,
};
const EXACT: Tiers = Tiers {
    exact: true,
    prefix: false,
    stem: false,
    typo: false,
};

/// Which fields a term clause covers.
#[derive(Clone, Copy)]
enum Sel<'a> {
    All,
    Quoted,
    NonQuoted,
    /// one word field and its bigram field (annotation)
    Only(&'a str, &'a str),
}

impl Sel<'_> {
    fn keep(&self, name: &str) -> bool {
        match *self {
            Sel::All => true,
            Sel::Quoted => matches!(name, "quoted" | "ng_quoted"),
            Sel::NonQuoted => !matches!(name, "quoted" | "ng_quoted"),
            Sel::Only(w, n) => name == w || (!n.is_empty() && name == n),
        }
    }
}

/// Zero-score filters of one query.
#[derive(Clone, Copy, Default)]
struct Filters {
    /// search_key range [lo, hi)
    skey: Option<(u64, u64)>,
    account: u64,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ---- people and affinity ------------------------------------------------------------------------

struct Side {
    dir: RoDir,
    index: Index,
    reader: IndexReader,
}

/// A Tantivy error on `dir`: [`OpenError::Corrupt`] when a file failed its integrity check.
fn side_err(dir: &RoDir, e: tantivy::TantivyError) -> OpenError {
    match dir.corrupt() {
        Some(c) => OpenError::Corrupt(c),
        None => e.into(),
    }
}

fn open_side(path: &Path) -> Result<Side, OpenError> {
    let dir = RoDir::open(path).map_err(|e| OpenError::Io(e.to_string()))?;
    let index = Index::open(dir.clone()).map_err(|e| side_err(&dir, e))?;
    mailsearch_core::register(&index);
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(|e| side_err(&dir, e))?;
    Ok(Side { dir, index, reader })
}

/// The segment files a cold query reads most (term dictionaries, fast fields, fieldnorms), in prefetch order.
/// At 100k a cold first page reads 14.2 MiB of term, 3.5 of fast and 0.7 of fieldnorm files; postings and
/// positions are read only where the query's terms are.
pub const PREFETCH_EXTS: [&str; 3] = ["term", "fast", "fieldnorm"];
/// Range of one `POSIX_FADV_WILLNEED` call in [`Engine::prefetch`].
pub const PREFETCH_CHUNK: u64 = 2 << 20;

/// [`Engine::prefetch`]'s thread: readahead for every file, then a read of the pages still not in the page
/// cache. Errors are ignored (a file gone after a reload is no longer needed).
fn prefetch_files(files: &[(PathBuf, u64)]) {
    use std::os::fd::AsRawFd;
    let open: Vec<(std::fs::File, u64)> = files
        .iter()
        .filter(|(_, n)| *n > 0)
        .filter_map(|(p, n)| Some((std::fs::File::open(p).ok()?, *n)))
        .collect();
    for (f, n) in &open {
        let mut o = 0;
        while o < *n {
            // SAFETY: a plain syscall on an open descriptor.
            unsafe {
                libc::posix_fadvise(
                    f.as_raw_fd(),
                    o as libc::off_t,
                    PREFETCH_CHUNK as libc::off_t,
                    libc::POSIX_FADV_WILLNEED,
                )
            };
            o += PREFETCH_CHUNK;
        }
    }
    let mut buf = vec![0u8; 256 << 10];
    for (f, n) in &open {
        read_missing(f, *n, &mut buf);
    }
}

/// Reads the pages of `f` (`n` bytes) that are not in the page cache (`mincore`), so a warm prefetch reads
/// nothing.
fn read_missing(f: &std::fs::File, n: u64, buf: &mut [u8]) {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let len = n as usize;
    let pages = n.div_ceil(page) as usize;
    let mut v = vec![0u8; pages];
    // SAFETY: a shared read-only mapping of an open file that is only passed to mincore (never read, so no
    // page is faulted into this process) and unmapped before the pointer goes out of scope.
    let resident = unsafe {
        let m = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        );
        if m == libc::MAP_FAILED {
            false
        } else {
            let ok = libc::mincore(m, len, v.as_mut_ptr()) == 0;
            libc::munmap(m, len);
            ok
        }
    };
    if !resident {
        v.fill(0);
    }
    let mut i = 0;
    while i < pages {
        if v[i] & 1 == 1 {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < pages && v[j] & 1 == 0 {
            j += 1;
        }
        let (mut o, end) = (i as u64 * page, (j as u64 * page).min(n));
        while o < end {
            let k = ((end - o) as usize).min(buf.len());
            match f.read_at(&mut buf[..k], o) {
                Ok(0) => break,
                Ok(r) => o += r as u64,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
        i = j;
    }
}

struct People {
    side: Side,
    f: PeopleFields,
}

/// Affinity of every correspondent (from the people index).
#[derive(Default)]
struct Aff {
    /// sender address hash → A
    a: HashMap<u64, f64>,
    /// the 50 correspondents the user sent most to (address hashes)
    top50: Arc<HashSet<u64>>,
    /// correspondents.id → (address hash, A, message count)
    by_id: HashMap<u64, (u64, f64, u64)>,
}

fn affinity_of(sent: u64, replied: u64, contact: bool) -> f64 {
    let mut a = ((1.0 + sent as f64).ln() / 51f64.ln()).min(1.0);
    if replied > 0 {
        a = a.max(0.5);
    }
    if contact {
        a = 1.0;
    }
    a
}

// ---- engine -------------------------------------------------------------------------------------

pub struct Engine {
    root: PathBuf,
    idx: u32,
    mail: Side,
    people: Option<People>,
    f: Fields,
    an: TextAnalyzer,
    snow: Snowball,
    zones: HashMap<SegmentId, Arc<(Vec<u64>, u64)>>,
    excluded: Option<Arc<KeySet>>,
    bits: Arc<HashMap<SegmentId, Arc<BitSet>>>,
    owner: HashSet<u64>,
    /// affinity per account scope (`None` = every account), for the loaded people generation
    aff: HashMap<Option<Vec<u64>>, Arc<Aff>>,
    aff_accounts: Option<Vec<u64>>,
    cancel: Arc<AtomicBool>,
    /// a [`Engine::prefetch`] thread is running
    prefetching: Arc<AtomicBool>,
}

impl Engine {
    /// Opens the current index of a search root (`$data/search`) read-only after the fail-closed
    /// version check.
    pub fn open(root: &Path) -> Result<Engine, OpenError> {
        let n = read_current(root)
            .ok_or_else(|| OpenError::Io(format!("{}: no valid CURRENT", root.display())))?;
        let dir = root.join(idx_name(n));
        if !dir.is_dir() {
            return Err(OpenError::Io(format!("{}: missing", dir.display())));
        }
        check_stamp(&dir).map_err(OpenError::Version)?;
        let mail = open_side(&dir.join(MAIL_DIR))?;
        let f = Fields::new(&mail.index.schema())?;
        let people = if dir.join(PEOPLE_DIR).join("meta.json").exists() {
            let side = open_side(&dir.join(PEOPLE_DIR))?;
            let f = PeopleFields::new(&side.index.schema())?;
            Some(People { side, f })
        } else {
            None
        };
        Ok(Engine {
            root: root.to_path_buf(),
            idx: n,
            mail,
            people,
            f,
            an: mail_analyzer(),
            snow: Snowball::new(&[]),
            zones: HashMap::new(),
            excluded: None,
            bits: Arc::new(HashMap::new()),
            owner: HashSet::new(),
            aff: HashMap::new(),
            aff_accounts: None,
            cancel: Arc::new(AtomicBool::new(false)),
            prefetching: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The flag [`Engine::search`] checks; set it from any thread to cancel the running search.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Picks up the newest commit, or the new `idx-N` after a rebuild switch, then releases the
    /// pages of the old mapping. Returns the lease `(dir, gen)` to report as ReaderAt; `gen` is read
    /// before reloading, so the reader holds nothing older than it.
    pub fn reload(&mut self) -> Result<(u32, u64), OpenError> {
        if read_current(&self.root).is_some_and(|n| n != self.idx) {
            let mut e = Engine::open(&self.root)?;
            let generation = e.mail.index.load_metas()?.opstamp;
            e.excluded = self.excluded.take();
            e.owner = std::mem::take(&mut self.owner);
            e.snow = std::mem::replace(&mut self.snow, Snowball::new(&[]));
            e.cancel = self.cancel.clone();
            e.prefetching = self.prefetching.clone();
            e.aff_accounts = self.aff_accounts.take();
            *self = e;
            self.release();
            return Ok((self.idx, generation));
        }
        let generation = self.mail.index.load_metas()?.opstamp;
        self.mail
            .reader
            .reload()
            .map_err(|e| side_err(&self.mail.dir, e))?;
        if let Some(p) = &self.people {
            p.side
                .reader
                .reload()
                .map_err(|e| side_err(&p.side.dir, e))?;
        }
        self.aff.clear();
        self.release();
        Ok((self.idx, generation))
    }

    /// `madvise(MADV_DONTNEED)` every mapping, then (glibc) `malloc_trim`. Returns the bytes advised.
    pub fn release(&self) -> u64 {
        let n = self.mail.dir.advise_all()
            + self
                .people
                .as_ref()
                .map(|p| p.side.dir.advise_all())
                .unwrap_or(0);
        #[cfg(target_env = "gnu")]
        // SAFETY: malloc_trim only returns free heap pages to the kernel.
        unsafe {
            libc::malloc_trim(0);
        }
        n
    }

    /// Reads the loaded segments' term dictionaries, fast fields and fieldnorms ([`PREFETCH_EXTS`], mail and
    /// people) into the page cache on a background thread, so the first query after the cache was dropped
    /// does not fault them in from disk one page at a time. The thread queues the whole of every file with
    /// `POSIX_FADV_WILLNEED` in [`PREFETCH_CHUNK`] steps (one call reads ahead at most the device's
    /// largest request, 2 MiB here), then reads what `mincore` still shows missing. It reads through file
    /// descriptors, not the reader's mappings, so nothing is mapped into this process, and a warm prefetch
    /// costs only the page cache lookups. Returns the bytes it will read and the thread, or `None` while an earlier
    /// prefetch is still running.
    pub fn prefetch(&self) -> std::io::Result<Option<(u64, std::thread::JoinHandle<()>)>> {
        if self.prefetching.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let mut files = self.mail.dir.mapped_files(&PREFETCH_EXTS);
        if let Some(p) = &self.people {
            files.extend(p.side.dir.mapped_files(&PREFETCH_EXTS));
        }
        let bytes = files.iter().map(|(_, n)| n).sum();
        let busy = self.prefetching.clone();
        let t = std::thread::Builder::new()
            .name("mailsearch-prefetch".into())
            .spawn(move || {
                prefetch_files(&files);
                busy.store(false, Ordering::Release);
            });
        match t {
            Ok(t) => Ok(Some((bytes, t))),
            Err(e) => {
                self.prefetching.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    pub fn num_docs(&self) -> u64 {
        self.mail.reader.searcher().num_docs()
    }

    /// The index directory number loaded (`idx-N`).
    pub fn dir_number(&self) -> u32 {
        self.idx
    }

    /// The excluded message id set (Trash, Junk, out-of-scope folders); replaces the previous set.
    pub fn set_excluded(&mut self, mids: KeySet) {
        self.excluded = (!mids.is_empty()).then(|| Arc::new(mids));
        self.bits = Arc::new(HashMap::new());
    }

    /// The user's own addresses (own-sent feature O); replaces the previous set.
    pub fn set_owner<S: AsRef<str>>(&mut self, addrs: &[S]) {
        self.owner = addrs
            .iter()
            .map(|a| fnv1a64(a.as_ref().trim().to_lowercase().as_bytes()))
            .collect();
    }

    /// Snowball languages besides English for the stem tier (language codes, e.g. "de").
    pub fn set_languages(&mut self, langs: &[&str]) {
        self.snow = Snowball::new(langs);
    }

    /// Restricts sender affinity (feature A) and the top-50 correspondents (the extra Top-hits
    /// candidates) to the Person rows of these accounts; `None` = every account (the default: one
    /// user). For an evaluation index holding many users' mailboxes. Cached per account set.
    pub fn set_affinity_accounts(&mut self, accounts: Option<KeySet>) {
        self.aff_accounts = accounts.map(|s| {
            let mut v: Vec<u64> = s.into_iter().collect();
            v.sort_unstable();
            v
        });
    }

    fn check_cancel(&self) -> SResult<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(SearchError::Cancelled)
        } else {
            Ok(())
        }
    }

    // ---- per-segment structures ----

    fn ensure_zones(&mut self, searcher: &Searcher) {
        let segs = searcher.segment_readers();
        let live: HashSet<SegmentId> = segs.iter().map(|s| s.segment_id()).collect();
        self.zones.retain(|k, _| live.contains(k));
        let mut buf = vec![0u64; ZONE as usize];
        for seg in segs {
            if self.zones.contains_key(&seg.segment_id()) {
                continue;
            }
            let Ok(c) = col(seg, "skey") else { continue };
            let n = seg.max_doc();
            let mut zm = Vec::with_capacity((n / ZONE + 1) as usize);
            let mut d = 0u32;
            while d < n {
                let k = (n - d).min(ZONE) as usize;
                c.get_range(d as u64, &mut buf[..k]);
                zm.push(buf[..k].iter().copied().max().unwrap_or(0));
                d += k as u32;
            }
            let smax = zm.iter().copied().max().unwrap_or(0);
            self.zones.insert(seg.segment_id(), Arc::new((zm, smax)));
        }
    }

    fn ensure_bits(&mut self, searcher: &Searcher) {
        let Some(keys) = self.excluded.clone() else {
            return;
        };
        let segs = searcher.segment_readers();
        if segs.iter().all(|s| self.bits.contains_key(&s.segment_id())) {
            return;
        }
        let live: HashSet<SegmentId> = segs.iter().map(|s| s.segment_id()).collect();
        let mut m: HashMap<SegmentId, Arc<BitSet>> = (*self.bits).clone();
        m.retain(|k, _| live.contains(k));
        let mut buf = vec![0u64; 4096];
        for seg in segs {
            if m.contains_key(&seg.segment_id()) {
                continue;
            }
            let Ok(c) = col(seg, "mid") else { continue };
            let n = seg.max_doc();
            let mut bs = BitSet::with_max_value(n.max(1));
            let mut d = 0u32;
            while d < n {
                let k = (n - d).min(4096) as usize;
                c.get_range(d as u64, &mut buf[..k]);
                for (j, id) in buf[..k].iter().enumerate() {
                    if keys.contains(id) {
                        bs.insert(d + j as u32);
                    }
                }
                d += k as u32;
            }
            m.insert(seg.segment_id(), Arc::new(bs));
        }
        self.bits = Arc::new(m);
    }

    /// One collection pass; cancellable between segments and every [`CANCEL_EVERY`] documents.
    fn run(&self, searcher: &Searcher, query: &dyn TQuery, coll: &Coll) -> SResult<Fruit> {
        let es = if coll.scoring {
            EnableScoring::enabled_from_searcher(searcher)
        } else {
            EnableScoring::disabled_from_searcher(searcher)
        };
        let weight = query.weight(es)?;
        let segs = searcher.segment_readers();
        let mut order: Vec<(u64, usize)> = segs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let m = if coll.early {
                    self.zones
                        .get(&s.segment_id())
                        .map(|z| z.1)
                        .unwrap_or(u64::MAX)
                } else {
                    0
                };
                (m, i)
            })
            .collect();
        if coll.early {
            order.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        }
        let mut acc = Acc::default();
        let mut skipped = false;
        for (smax, i) in order {
            self.check_cancel()?;
            let seg = &segs[i];
            if coll.early && coll.can_skip(&acc, smax) {
                skipped = true;
                continue;
            }
            let cols = SegCols::new(seg)?;
            let z = if coll.early {
                self.zones.get(&seg.segment_id()).cloned()
            } else {
                None
            };
            let excl = coll
                .bits
                .as_ref()
                .and_then(|m| m.get(&seg.segment_id()).cloned());
            let alive = seg.alive_bitset();
            let max_doc = seg.max_doc();
            let mut sc = weight.scorer(seg, 1.0)?;
            let mut doc = sc.doc();
            let mut visited = 0u32;
            while doc != TERMINATED {
                visited += 1;
                if visited.is_multiple_of(CANCEL_EVERY) {
                    self.check_cancel()?;
                }
                if let Some(z) = &z {
                    let b = (doc / ZONE) as usize;
                    if coll.can_skip(&acc, z.0[b]) {
                        skipped = true;
                        let next = (b as u32 + 1) * ZONE;
                        if next >= max_doc {
                            break;
                        }
                        doc = sc.seek(next);
                        continue;
                    }
                }
                if alive.is_none_or(|a| a.is_alive(doc))
                    && excl.as_ref().is_none_or(|b| !b.contains(doc))
                {
                    let s = if coll.scoring { sc.score() } else { 0.0 };
                    coll.collect(&mut acc, &cols, i as u32, doc, s);
                }
                doc = sc.advance();
            }
        }
        // the top-50 correspondents' matches anywhere (the walk may have skipped some)
        if skipped && let Some(x) = &coll.extra {
            let addrs: Vec<u64> = x.iter().copied().collect();
            let q2 = BooleanQuery::new(vec![
                (Occur::Must, query.box_clone()),
                (Occur::Must, self.sender_filter(&addrs)),
            ]);
            let c2 = Coll {
                cap: 0,
                extra: Some(x.clone()),
                scoring: coll.scoring,
                bits: coll.bits.clone(),
                collapse: false,
                key_cap: 0,
                early: false,
                count_need: 0,
            };
            acc.ex = self.run(searcher, &q2, &c2)?.extra;
        }
        let distinct = if coll.collapse {
            acc.distinct
        } else {
            acc.count
        };
        let mut newest = acc.heap.into_vec();
        newest.sort_unstable_by(|a, b| b.skey.cmp(&a.skey).then(b.mid.cmp(&a.mid)));
        Ok(Fruit {
            count: acc.count,
            distinct,
            newest,
            extra: acc.ex,
            skipped,
        })
    }

    // ---- query compilation ----

    fn compile(&mut self, q: &Query, o: &Opts) -> Vec<CClause> {
        let mut out = Vec::new();
        for cl in &q.clauses {
            let fields = cl.fields & F_TEXT;
            if fields == 0 {
                continue;
            }
            let CompiledWord { cjk, word } = compile_word(&mut self.an, &cl.text);
            let mut terms: Vec<T> = cjk
                .into_iter()
                .filter(|b| !b.is_empty())
                .map(|bigrams| T {
                    fields,
                    tk: TK::Cjk { bigrams },
                    partial: cl.partial,
                })
                .collect();
            let explicit_prefix = cl.mode == Mode::Prefix;
            match word {
                None => {}
                Some(QueryToken::Phrase(parts)) => {
                    let tk = match cl.mode {
                        Mode::Near(slop) => TK::Near { parts, slop },
                        Mode::Prefix => TK::Phrase {
                            parts,
                            last_prefix: true,
                        },
                        Mode::Default => TK::Phrase {
                            parts,
                            last_prefix: cl.partial && !cl.negate,
                        },
                        Mode::Exact | Mode::Phrase => TK::Phrase {
                            parts,
                            last_prefix: false,
                        },
                    };
                    terms.push(T {
                        fields,
                        tk,
                        partial: cl.partial,
                    });
                }
                Some(QueryToken::Term(t)) => {
                    let n = t.chars().count();
                    if n == 1 && cl.partial && cl.mode == Mode::Default && !cl.negate {
                        // a 1-letter word being typed feeds the People suggestions only
                        if terms.is_empty() {
                            continue;
                        }
                    } else {
                        let prefix = match cl.mode {
                            Mode::Prefix => true,
                            Mode::Exact | Mode::Phrase | Mode::Near(_) => false,
                            Mode::Default => !cl.negate && n >= 2 && (!q.typing || cl.partial),
                        };
                        let short = q.typing
                            && cl.partial
                            && prefix
                            && !explicit_prefix
                            && n <= o.pfx_short
                            && fields == F_TEXT;
                        terms.push(T {
                            fields,
                            tk: TK::Tiers {
                                t,
                                prefix,
                                short,
                                expand: cl.mode == Mode::Default && !cl.negate,
                                stems: Vec::new(),
                                typo: false,
                            },
                            partial: cl.partial,
                        });
                    }
                }
            }
            if terms.is_empty() {
                continue;
            }
            out.push(CClause {
                terms,
                negate: cl.negate,
                group: cl.group,
            });
        }
        out
    }

    fn word_fields(&self, mask: u32, sel: Sel) -> Vec<(Field, &'static str)> {
        self.f
            .text
            .iter()
            .zip(FIELD_BITS.iter())
            .filter(|(f, (bit, _, _))| mask & bit != 0 && sel.keep(f.1))
            .map(|(f, _)| *f)
            .collect()
    }

    fn ng_fields(&self, mask: u32, sel: Sel) -> Vec<(Field, &'static str)> {
        let mut v: Vec<(Field, &'static str)> = Vec::new();
        for (bit, _, ng) in FIELD_BITS {
            if mask & bit == 0 || ng.is_empty() || !sel.keep(ng) {
                continue;
            }
            if let Some(f) = self.f.ng.iter().find(|x| x.1 == ng)
                && !v.iter().any(|x| x.1 == ng)
            {
                v.push(*f);
            }
        }
        v
    }

    /// Documents containing `t` in any word field (the vocabulary df).
    fn df(&self, searcher: &Searcher, t: &str) -> u64 {
        for (f, _) in &self.f.text {
            let n = searcher
                .doc_freq(&Term::from_field_text(*f, t))
                .unwrap_or(0);
            if n > 0 {
                return n;
            }
        }
        0
    }

    /// S3 stems: vocabulary terms (subject, body, quoted dictionaries of every segment) sharing the
    /// Snowball stem's prefix whose stem is equal; not the word itself nor its prefix completions;
    /// ≤ 8 by document frequency.
    fn stems(&self, searcher: &Searcher, t: &str) -> Vec<String> {
        let mut df: HashMap<String, u64> = HashMap::new();
        let fields: Vec<Field> = self
            .f
            .text
            .iter()
            .filter(|(_, n)| matches!(*n, "subject" | "body" | "quoted"))
            .map(|x| x.0)
            .collect();
        for (st, stem) in self.snow.stems(t) {
            if stem.chars().count() < 3 {
                continue;
            }
            let hi = format!("{stem}\u{10FFFF}");
            for seg in searcher.segment_readers() {
                for f in &fields {
                    let Ok(inv) = seg.inverted_index(*f) else {
                        continue;
                    };
                    let Ok(mut strm) = inv
                        .terms()
                        .range()
                        .ge(stem.as_bytes())
                        .lt(hi.as_bytes())
                        .into_stream()
                    else {
                        continue;
                    };
                    let mut k = 0;
                    while strm.advance() && k < 3000 {
                        k += 1;
                        let Ok(term) = std::str::from_utf8(strm.key()) else {
                            continue;
                        };
                        if term == t || term.starts_with(t) {
                            continue;
                        }
                        let d = strm.value().doc_freq as u64;
                        if let Some(e) = df.get_mut(term) {
                            *e = (*e).max(d);
                            continue;
                        }
                        if st.stem(term) == stem.as_str() {
                            df.insert(term.to_string(), d);
                        }
                    }
                }
            }
        }
        let mut v: Vec<(String, u64)> = df.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(8);
        v.into_iter().map(|x| x.0).collect()
    }

    /// Compiles the clauses and expands stems and the first typo trigger (pause/Enter only).
    fn prepare(
        &mut self,
        q: &Query,
        o: &Opts,
        searcher: &Searcher,
    ) -> (Vec<CClause>, usize, usize) {
        let mut cls = self.compile(q, o);
        let (mut n_stem, mut n_typo) = (0, 0);
        if q.typing {
            return (cls, 0, 0);
        }
        for c in cls.iter_mut().filter(|c| !c.negate) {
            for term in c.terms.iter_mut() {
                let partial = term.partial;
                let TK::Tiers {
                    t,
                    expand,
                    stems,
                    typo,
                    ..
                } = &mut term.tk
                else {
                    continue;
                };
                if partial || !*expand || !eligible(t) {
                    continue;
                }
                if o.stems {
                    *stems = self.stems(searcher, t);
                    n_stem += stems.len();
                }
                // typo trigger 1: absent from the vocabulary
                if o.typo && self.df(searcher, t) == 0 {
                    *typo = true;
                    n_typo += 1;
                }
            }
        }
        (cls, n_stem, n_typo)
    }

    fn term_query(f: Field, t: &str, rec: IndexRecordOption) -> Box<dyn TQuery> {
        Box::new(TermQuery::new(Term::from_field_text(f, t), rec))
    }

    /// The query of one term, restricted to `sel`'s fields and `tiers`.
    fn term_q(&self, t: &T, tiers: Tiers, o: &Opts, sel: Sel) -> Box<dyn TQuery> {
        let mut subs: Vec<(Occur, Box<dyn TQuery>)> = Vec::new();
        let boost = |q: Box<dyn TQuery>, w: f32| -> (Occur, Box<dyn TQuery>) {
            (Occur::Should, Box::new(BoostQuery::new(q, w)))
        };
        match &t.tk {
            TK::Tiers {
                t: w,
                prefix,
                short,
                stems,
                typo,
                ..
            } => {
                if *short {
                    if tiers.prefix && sel.keep("hpfx") {
                        let q = ConstScoreQuery::new(
                            Self::term_query(self.f.hpfx, w, IndexRecordOption::Basic),
                            1.0,
                        );
                        subs.push(boost(Box::new(q), o.colw.subject * o.prefix_w));
                    }
                    if tiers.exact {
                        for (f, name) in self.word_fields(t.fields, sel) {
                            subs.push(boost(
                                Self::term_query(f, w, IndexRecordOption::WithFreqs),
                                o.colw.of(name),
                            ));
                        }
                    }
                    return Box::new(BooleanQuery::new(subs));
                }
                for (f, name) in self.word_fields(t.fields, sel) {
                    let cw = o.colw.of(name);
                    if tiers.exact {
                        subs.push(boost(
                            Self::term_query(f, w, IndexRecordOption::WithFreqs),
                            cw,
                        ));
                    }
                    if *prefix && tiers.prefix {
                        subs.push(boost(
                            Box::new(FuzzyTermQuery::new_prefix(
                                Term::from_field_text(f, w),
                                0,
                                false,
                            )),
                            cw * o.prefix_w,
                        ));
                    }
                    if tiers.stem {
                        for s in stems {
                            subs.push(boost(
                                Self::term_query(f, s, IndexRecordOption::WithFreqs),
                                cw * o.stem_w,
                            ));
                        }
                    }
                    if *typo && tiers.typo {
                        // The design says ≤ 3 candidates per term; the measured engine uses the
                        // unbounded FuzzyTermQuery (distance 1, transpositions), kept as measured.
                        subs.push(boost(
                            Box::new(FuzzyTermQuery::new(Term::from_field_text(f, w), 1, true)),
                            cw * o.typo_w,
                        ));
                    }
                }
            }
            TK::Phrase { parts, last_prefix } => {
                let use_prefix = *last_prefix && tiers.prefix;
                if use_prefix || tiers.exact {
                    for (f, name) in self.word_fields(t.fields, sel) {
                        let terms: Vec<Term> =
                            parts.iter().map(|p| Term::from_field_text(f, p)).collect();
                        let q: Box<dyn TQuery> = if use_prefix {
                            Box::new(PhrasePrefixQuery::new(terms))
                        } else if *last_prefix {
                            continue;
                        } else {
                            Box::new(PhraseQuery::new(terms))
                        };
                        subs.push(boost(q, o.colw.of(name)));
                    }
                }
            }
            TK::Near { parts, slop } => {
                if tiers.exact {
                    // Tantivy's slop is order-preserving (a transposition costs 2): one phrase per order
                    for (f, name) in self.word_fields(t.fields, sel) {
                        for rev in [false, true] {
                            let mut terms: Vec<Term> =
                                parts.iter().map(|p| Term::from_field_text(f, p)).collect();
                            if rev {
                                terms.reverse();
                            }
                            let mut q = PhraseQuery::new(terms);
                            q.set_slop(*slop);
                            subs.push(boost(Box::new(q), o.colw.of(name)));
                        }
                    }
                }
            }
            TK::Cjk { bigrams } => {
                if tiers.exact {
                    for (f, name) in self.ng_fields(t.fields, sel) {
                        let q: Box<dyn TQuery> = if bigrams.len() == 1 {
                            Self::term_query(f, &bigrams[0], IndexRecordOption::WithFreqs)
                        } else {
                            Box::new(PhraseQuery::new(
                                bigrams
                                    .iter()
                                    .map(|b| Term::from_field_text(f, b))
                                    .collect(),
                            ))
                        };
                        subs.push(boost(q, o.colw.of(name)));
                    }
                }
            }
        }
        Box::new(BooleanQuery::new(subs))
    }

    fn clause_q(&self, c: &CClause, tiers: Tiers, o: &Opts, sel: Sel) -> Box<dyn TQuery> {
        if c.terms.len() == 1 {
            return self.term_q(&c.terms[0], tiers, o, sel);
        }
        Box::new(BooleanQuery::new(
            c.terms
                .iter()
                .map(|t| (Occur::Must, self.term_q(t, tiers, o, sel)))
                .collect(),
        ))
    }

    fn filter_qs(&self, fl: &Filters) -> Vec<(Occur, Box<dyn TQuery>)> {
        let mut v: Vec<(Occur, Box<dyn TQuery>)> = Vec::new();
        let zero =
            |q: Box<dyn TQuery>| -> Box<dyn TQuery> { Box::new(ConstScoreQuery::new(q, 0.0)) };
        if let Some((lo, hi)) = fl.skey {
            v.push((
                Occur::Must,
                zero(Box::new(RangeQuery::new(
                    Bound::Included(Term::from_field_u64(self.f.skey, lo)),
                    Bound::Excluded(Term::from_field_u64(self.f.skey, hi)),
                ))),
            ));
        }
        if fl.account != 0 {
            v.push((
                Occur::Must,
                zero(Box::new(TermQuery::new(
                    Term::from_field_u64(self.f.account, fl.account),
                    IndexRecordOption::Basic,
                ))),
            ));
        }
        v
    }

    /// The whole query: AND of clauses and OR groups, NOT clauses, filters. `None` when no positive
    /// clause remains.
    fn build(
        &self,
        cls: &[CClause],
        tiers: Tiers,
        fl: &Filters,
        o: &Opts,
        extra: Option<Box<dyn TQuery>>,
    ) -> Option<Box<dyn TQuery>> {
        let mut subs: Vec<(Occur, Box<dyn TQuery>)> = Vec::new();
        let mut groups: Vec<(u32, Subs)> = Vec::new();
        let mut positive = false;
        for c in cls {
            let q = if c.negate {
                let q = self.clause_q(c, ALL, o, Sel::All);
                if c.group == 0 {
                    subs.push((Occur::MustNot, q));
                    continue;
                }
                // "NOT x" as one alternative of an OR group
                Box::new(BooleanQuery::new(vec![
                    (
                        Occur::Must,
                        Box::new(ConstScoreQuery::new(Box::new(AllQuery), 0.0)) as Box<dyn TQuery>,
                    ),
                    (Occur::MustNot, q),
                ])) as Box<dyn TQuery>
            } else {
                positive = true;
                self.clause_q(c, tiers, o, Sel::All)
            };
            if c.group == 0 {
                subs.push((Occur::Must, q));
            } else if let Some(g) = groups.iter_mut().find(|g| g.0 == c.group) {
                g.1.push((Occur::Should, q));
            } else {
                groups.push((c.group, vec![(Occur::Should, q)]));
            }
        }
        if let Some(e) = extra {
            positive = true;
            subs.push((Occur::Must, e));
        }
        if !positive {
            return None;
        }
        for (_, g) in groups {
            subs.push((Occur::Must, Box::new(BooleanQuery::new(g))));
        }
        subs.extend(self.filter_qs(fl));
        Some(Box::new(BooleanQuery::new(subs)))
    }

    /// Clauses without the clause at `skip` (people-first: the fuzzy person word replaced by its
    /// sender filter).
    fn without(cls: &[CClause], skip: (usize, usize)) -> Vec<CClause> {
        let mut v = cls.to_vec();
        v[skip.0].terms.remove(skip.1);
        if v[skip.0].terms.is_empty() {
            v.remove(skip.0);
        }
        v
    }

    fn sender_filter(&self, addrs: &[u64]) -> Box<dyn TQuery> {
        let q = TermSetQuery::new(
            addrs
                .iter()
                .map(|a| Term::from_field_u64(self.f.from_h, *a)),
        );
        Box::new(ConstScoreQuery::new(Box::new(q), 0.0))
    }

    // ---- people ----

    fn affinity(&mut self) -> Arc<Aff> {
        if let Some(a) = self.aff.get(&self.aff_accounts) {
            return a.clone();
        }
        let mut aff = Aff::default();
        if let Some(p) = &self.people {
            let s = p.side.reader.searcher();
            let mut addr: HashMap<u64, (u64, bool)> = HashMap::new(); // id → addr_h, in scope
            let mut corr: HashMap<u64, (u64, u64, u64, bool)> = HashMap::new(); // id → sent, replied, n, contact
            for seg in s.segment_readers() {
                let cols = (|| -> tantivy::Result<_> {
                    Ok((
                        col(seg, "kind")?,
                        col(seg, "account")?,
                        col(seg, "id")?,
                        col(seg, "addr_h")?,
                        col(seg, "sent_to")?,
                        col(seg, "replied_to")?,
                        col(seg, "received_from")?,
                        col(seg, "cc_with")?,
                        col(seg, "is_contact")?,
                    ))
                })();
                let Ok((kind, account, id, ah, sent, rep, recv, cc, contact)) = cols else {
                    continue;
                };
                for d in 0..seg.max_doc() {
                    if seg.is_deleted(d) {
                        continue;
                    }
                    let i = id.get_val(d);
                    if kind.get_val(d) == 0 {
                        let scope = self
                            .aff_accounts
                            .as_ref()
                            .is_none_or(|v| v.binary_search(&account.get_val(d)).is_ok());
                        addr.insert(i, (ah.get_val(d), scope));
                    } else {
                        let n = sent.get_val(d) + recv.get_val(d) + cc.get_val(d);
                        corr.insert(
                            i,
                            (sent.get_val(d), rep.get_val(d), n, contact.get_val(d) != 0),
                        );
                    }
                }
            }
            let mut top: Vec<(u64, u64)> = Vec::new();
            for (id, (ah, scope)) in &addr {
                let (sent, rep, n, contact) = corr.get(id).copied().unwrap_or_default();
                if !scope {
                    // outside the affinity accounts: suggestions and people-first only
                    aff.by_id.insert(*id, (*ah, 0.0, n));
                    continue;
                }
                let a = affinity_of(sent, rep, contact);
                let e = aff.a.entry(*ah).or_insert(0.0);
                *e = e.max(a);
                aff.by_id.insert(*id, (*ah, a, n));
                if sent > 0 {
                    top.push((*ah, sent));
                }
            }
            top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let mut seen = HashSet::new();
            top.retain(|x| seen.insert(x.0));
            aff.top50 = Arc::new(top.iter().take(50).map(|x| x.0).collect());
        }
        let a = Arc::new(aff);
        self.aff.insert(self.aff_accounts.clone(), a.clone());
        a
    }

    /// Person documents matching `q` (correspondents.id), best first: by affinity then message
    /// count (`by_affinity`, suggestions) or by message count (people-first); the best `limit` only when
    /// given. A short prefix matches nearly every person (`c`: every `.com` address), so each key is looked
    /// up once and only the best `limit` are sorted.
    fn people_match(
        &self,
        q: Box<dyn TQuery>,
        aff: &Aff,
        by_affinity: bool,
        limit: Option<usize>,
    ) -> SResult<Vec<u64>> {
        let Some(p) = &self.people else {
            return Ok(Vec::new());
        };
        let s = p.side.reader.searcher();
        let q = BooleanQuery::new(vec![
            (Occur::Must, q),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(p.f.kind, 0),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let w = q.weight(EnableScoring::disabled_from_searcher(&s))?;
        let mut ids: Vec<u64> = Vec::new();
        for seg in s.segment_readers() {
            self.check_cancel()?;
            let id = col(seg, "id")?;
            let alive = seg.alive_bitset();
            let mut sc = w.scorer(seg, 1.0)?;
            let mut d = sc.doc();
            while d != TERMINATED {
                if alive.is_none_or(|a| a.is_alive(d)) {
                    ids.push(id.get_val(d));
                }
                d = sc.advance();
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let mut v: Vec<(f64, u64, u64)> = ids
            .into_iter()
            .map(|id| {
                let (_, a, n) = aff.by_id.get(&id).copied().unwrap_or((0, 0.0, 0));
                (if by_affinity { a } else { 0.0 }, n, id)
            })
            .collect();
        let best = |x: &(f64, u64, u64), y: &(f64, u64, u64)| {
            y.0.total_cmp(&x.0).then(y.1.cmp(&x.1)).then(x.2.cmp(&y.2))
        };
        if let Some(k) = limit
            && k < v.len()
        {
            v.select_nth_unstable_by(k, best);
            v.truncate(k);
        }
        v.sort_unstable_by(best);
        Ok(v.into_iter().map(|x| x.2).collect())
    }

    /// People-first resolution of one word: exact name token, else prefix (≥ 3), else fuzzy (≥ 4,
    /// not while typing). Sender address hashes of the top 5; kind 1/2/3, 0 = none.
    fn resolve_person(&self, t: &str, partial: bool, aff: &Aff) -> SResult<(Vec<u64>, u8)> {
        let Some(p) = &self.people else {
            return Ok((Vec::new(), 0));
        };
        let term = Term::from_field_text(p.f.names, t);
        let n = t.chars().count();
        // the top 5 distinct sender addresses
        let addrs = |ids: Vec<u64>| -> Vec<u64> {
            let mut seen = HashSet::new();
            ids.iter()
                .filter_map(|i| aff.by_id.get(i).map(|x| x.0))
                .filter(|a| seen.insert(*a))
                .take(5)
                .collect()
        };
        if !partial {
            let v = self.people_match(
                Box::new(TermQuery::new(term.clone(), IndexRecordOption::Basic)),
                aff,
                false,
                None,
            )?;
            if !v.is_empty() {
                return Ok((addrs(v), 1));
            }
        }
        if n >= 3 {
            let v = self.people_match(
                Box::new(FuzzyTermQuery::new_prefix(term.clone(), 0, false)),
                aff,
                false,
                None,
            )?;
            if !v.is_empty() {
                return Ok((addrs(v), 2));
            }
        }
        if n >= 4 && !partial {
            let d = if n >= 8 { 2 } else { 1 };
            let v = self.people_match(
                Box::new(FuzzyTermQuery::new(term, d, true)),
                aff,
                false,
                None,
            )?;
            if !v.is_empty() {
                return Ok((addrs(v), 3));
            }
        }
        Ok((Vec::new(), 0))
    }

    /// People suggestions: correspondents.id whose names or address parts match every
    /// word of `prefix` (the last as a prefix), best first (affinity, then message count).
    pub fn people(&mut self, prefix: &str, cap: usize) -> SResult<Vec<u64>> {
        self.cancel.store(false, Ordering::Relaxed);
        let Some(p) = &self.people else {
            return Ok(Vec::new());
        };
        let names = p.f.names;
        let toks = mailsearch_core::text::word_tokens_normalised(&mut self.an, &n1(prefix));
        if toks.is_empty() || cap == 0 {
            return Ok(Vec::new());
        }
        let last = toks.len() - 1;
        let subs: Vec<(Occur, Box<dyn TQuery>)> = toks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let term = Term::from_field_text(names, t);
                let q: Box<dyn TQuery> = if i == last {
                    Box::new(FuzzyTermQuery::new_prefix(term, 0, false))
                } else {
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                };
                (Occur::Must, q)
            })
            .collect();
        let aff = self.affinity();
        self.people_match(Box::new(BooleanQuery::new(subs)), &aff, true, Some(cap))
    }

    // ---- quoted-text fallback ----

    /// Quoted-text fallback: the extra text score of every candidate. A term's
    /// quoted-field match counts at `qfb_w` instead of the quoted weight when the candidate has no
    /// match for the term outside quoted text and no message of its conversation in the query's
    /// scope has one (conversation 0: the message alone). While typing, completed words only.
    #[allow(clippy::too_many_arguments)]
    fn quoted_fallback(
        &self,
        searcher: &Searcher,
        cls: &[CClause],
        fl: &Filters,
        o: &Opts,
        q: &Query,
        bits: &Option<Arc<HashMap<SegmentId, Arc<BitSet>>>>,
        cands: &[Cand],
        info: &mut Vec<(&'static str, f64)>,
    ) -> SResult<Vec<f32>> {
        let mut extra = vec![0f32; cands.len()];
        if o.qfb == Qfb::Off || cands.is_empty() || o.colw.quoted <= 0.0 {
            return Ok(extra);
        }
        let t0 = Instant::now();
        let mult = (o.qfb_w / o.colw.quoted - 1.0).max(0.0);
        let mut order: Vec<usize> = (0..cands.len()).collect();
        order.sort_unstable_by_key(|&k| (cands[k].addr.segment_ord, cands[k].addr.doc_id));
        let segs = searcher.segment_readers();
        let (mut n_qhit, mut n_fb) = (0usize, 0usize);
        for c in cls.iter().filter(|c| !c.negate) {
            for t in &c.terms {
                self.check_cancel()?;
                if (t.partial && q.typing) || t.fields & F_QUOTED == 0 {
                    continue;
                }
                // (1) candidates with a quoted match, and its score
                let wq = self
                    .term_q(t, ALL, o, Sel::Quoted)
                    .weight(EnableScoring::enabled_from_searcher(searcher))?;
                let mut qhit: Vec<(usize, f32)> = Vec::new();
                let mut i = 0;
                while i < order.len() {
                    let so = cands[order[i]].addr.segment_ord;
                    let mut sq = wq.scorer(&segs[so as usize], 1.0)?;
                    while i < order.len() && cands[order[i]].addr.segment_ord == so {
                        let k = order[i];
                        let d = cands[k].addr.doc_id;
                        if sq.doc() < d {
                            sq.seek(d);
                        }
                        if sq.doc() == d {
                            qhit.push((k, sq.score()));
                        }
                        i += 1;
                    }
                }
                n_qhit += qhit.len();
                if qhit.is_empty() {
                    continue;
                }
                // (2) conversation members in the query's scope; probe list: (segment, doc,
                // Some(qhit index) | None, conv)
                let qconv: Vec<u64> = qhit.iter().map(|(k, _)| cands[*k].conv).collect();
                let want: HashSet<u64> = qconv.iter().copied().filter(|c| *c != 0).collect();
                let mut probe: Vec<(u32, DocId, Option<usize>, u64)> = Vec::new();
                if !want.is_empty() {
                    let mut must: Vec<(Occur, Box<dyn TQuery>)> = vec![(
                        Occur::Must,
                        Box::new(TermSetQuery::new(
                            want.iter().map(|c| Term::from_field_u64(self.f.conv, *c)),
                        )),
                    )];
                    must.extend(self.filter_qs(fl));
                    let mq = BooleanQuery::new(must);
                    let wm = mq.weight(EnableScoring::disabled_from_searcher(searcher))?;
                    for (so, seg) in segs.iter().enumerate() {
                        let convc = col(seg, "conv")?;
                        let excl = bits
                            .as_ref()
                            .and_then(|m| m.get(&seg.segment_id()).cloned());
                        let alive = seg.alive_bitset();
                        let mut sc = wm.scorer(seg, 1.0)?;
                        let mut d = sc.doc();
                        while d != TERMINATED {
                            if alive.is_none_or(|a| a.is_alive(d))
                                && excl.as_ref().is_none_or(|b| !b.contains(d))
                            {
                                probe.push((so as u32, d, None, convc.get_val(d)));
                            }
                            d = sc.advance();
                        }
                    }
                }
                probe.extend(qhit.iter().enumerate().map(|(j, (k, _))| {
                    (
                        cands[*k].addr.segment_ord,
                        cands[*k].addr.doc_id,
                        Some(j),
                        qconv[j],
                    )
                }));
                probe.sort_unstable_by_key(|x| (x.0, x.1, x.2.is_some()));
                // (3) one walk of the non-quoted clause
                let wn = self
                    .term_q(t, ALL, o, Sel::NonQuoted)
                    .weight(EnableScoring::disabled_from_searcher(searcher))?;
                let mut nq_hit = vec![false; qhit.len()];
                let mut conv_nq: HashSet<u64> = HashSet::new();
                let mut i = 0;
                while i < probe.len() {
                    let so = probe[i].0;
                    let mut sn = wn.scorer(&segs[so as usize], 1.0)?;
                    while i < probe.len() && probe[i].0 == so {
                        let (_, d, j, cv) = probe[i];
                        if sn.doc() < d {
                            sn.seek(d);
                        }
                        if sn.doc() == d {
                            match j {
                                Some(j) => nq_hit[j] = true,
                                None => {
                                    conv_nq.insert(cv);
                                }
                            }
                        }
                        i += 1;
                    }
                }
                for (j, (k, s)) in qhit.iter().enumerate() {
                    if nq_hit[j] || (qconv[j] != 0 && conv_nq.contains(&qconv[j])) {
                        continue;
                    }
                    extra[*k] += mult * *s;
                    n_fb += 1;
                }
            }
        }
        info.push(("qfb_ms", ms(t0)));
        info.push(("qfb_quoted_match", n_qhit as f64));
        info.push(("qfb_applied", n_fb as f64));
        Ok(extra)
    }

    // ---- annotation ----

    /// For each address (any order), whether `weight` matches it.
    fn matches(searcher: &Searcher, w: &dyn Weight, addrs: &[DocAddress]) -> SResult<Vec<bool>> {
        let mut order: Vec<usize> = (0..addrs.len()).collect();
        order.sort_unstable_by_key(|&i| (addrs[i].segment_ord, addrs[i].doc_id));
        let mut out = vec![false; addrs.len()];
        let mut i = 0;
        while i < order.len() {
            let so = addrs[order[i]].segment_ord;
            let mut sc = w.scorer(searcher.segment_reader(so), 1.0)?;
            while i < order.len() && addrs[order[i]].segment_ord == so {
                let d = addrs[order[i]].doc_id;
                if sc.doc() < d {
                    sc.seek(d);
                }
                out[order[i]] = sc.doc() == d;
                i += 1;
            }
        }
        Ok(out)
    }

    /// Tier and field bits of each address: OR of the positive terms per tier and per field.
    fn annotate(
        &self,
        searcher: &Searcher,
        cls: &[CClause],
        o: &Opts,
        addrs: &[DocAddress],
    ) -> SResult<Vec<(u32, u32)>> {
        let mut out = vec![(0u32, 0u32); addrs.len()];
        if addrs.is_empty() {
            return Ok(out);
        }
        let pos: Vec<&T> = cls
            .iter()
            .filter(|c| !c.negate)
            .flat_map(|c| c.terms.iter())
            .collect();
        let or = |tiers: Tiers, sel: Sel| -> Box<dyn TQuery> {
            Box::new(BooleanQuery::new(
                pos.iter()
                    .map(|t| (Occur::Should, self.term_q(t, tiers, o, sel)))
                    .collect(),
            ))
        };
        let tier_masks = [
            (T_EXACT, EXACT),
            (
                T_PREFIX,
                Tiers {
                    exact: false,
                    prefix: true,
                    stem: false,
                    typo: false,
                },
            ),
            (
                T_STEM,
                Tiers {
                    exact: false,
                    prefix: false,
                    stem: true,
                    typo: false,
                },
            ),
            (
                T_TYPO,
                Tiers {
                    exact: false,
                    prefix: false,
                    stem: false,
                    typo: true,
                },
            ),
        ];
        let has = |f: &dyn Fn(&TK) -> bool| pos.iter().any(|t| f(&t.tk));
        for (bit, tiers) in tier_masks {
            let present = match bit {
                T_EXACT => true,
                T_PREFIX => has(&|tk| {
                    matches!(
                        tk,
                        TK::Tiers { prefix: true, .. }
                            | TK::Phrase {
                                last_prefix: true,
                                ..
                            }
                    )
                }),
                T_STEM => has(&|tk| matches!(tk, TK::Tiers { stems, .. } if !stems.is_empty())),
                _ => has(&|tk| matches!(tk, TK::Tiers { typo: true, .. })),
            };
            if !present {
                continue;
            }
            self.check_cancel()?;
            let w = or(tiers, Sel::All).weight(EnableScoring::disabled_from_searcher(searcher))?;
            for (k, m) in Self::matches(searcher, &*w, addrs)?.into_iter().enumerate() {
                if m {
                    out[k].0 |= bit;
                }
            }
        }
        let mask = pos.iter().fold(0u32, |m, t| m | t.fields);
        for (bit, word, ng) in FIELD_BITS {
            if mask & bit == 0 {
                continue;
            }
            self.check_cancel()?;
            let w = or(ALL, Sel::Only(word, ng))
                .weight(EnableScoring::disabled_from_searcher(searcher))?;
            for (k, m) in Self::matches(searcher, &*w, addrs)?.into_iter().enumerate() {
                if m {
                    out[k].1 |= bit;
                }
            }
        }
        Ok(out)
    }

    // ---- search ----

    /// One query. Clears the cancel flag first.
    pub fn search(&mut self, q: &Query, o: &Opts) -> SResult<Hits> {
        self.cancel.store(false, Ordering::Relaxed);
        let mut info = Vec::new();
        let t_all = Instant::now();
        let searcher = self.mail.reader.searcher();
        let (mut cls, n_stem, mut n_typo) = self.prepare(q, o, &searcher);
        info.push(("expand_ms", ms(t_all)));
        let fl = Filters {
            skey: q
                .date
                .map(|(lo, hi)| ((lo.max(0) as u64) << 20, (hi.max(0) as u64) << 20)),
            account: q.account,
        };
        let tb = Instant::now();
        let Some(mut query) = self.build(&cls, ALL, &fl, o, None) else {
            return Ok(Hits {
                info,
                ..Default::default()
            });
        };
        info.push(("build_ms", ms(tb)));
        let ts = Instant::now();
        if q.typing || q.before_key != 0 {
            self.ensure_zones(&searcher);
        }
        let bits = if q.exclude && self.excluded.is_some() {
            self.ensure_bits(&searcher);
            Some(self.bits.clone())
        } else {
            None
        };
        let aff = self.affinity();
        info.push(("setup_ms", ms(ts)));
        let rel_wanted = (q.relevance || o.rel_list) && !q.skip_list;
        let need_rank = (!q.skip_top && q.n_top > 0) || rel_wanted;
        let need_date = !q.skip_list && q.n_list > 0;
        let date_cap = if q.collapse {
            q.n_list * 2 + 16
        } else {
            q.n_list
        };
        let rank_cap = o.k_rank.max(q.n_top);
        let key_cap = o.count_cap + 1;
        let extra = need_rank.then(|| aff.top50.clone());
        let mk = |cap, scoring, extra: Option<Arc<HashSet<u64>>>, early, count_need| Coll {
            cap,
            extra,
            scoring,
            bits: bits.clone(),
            collapse: q.collapse,
            key_cap,
            early,
            count_need,
        };
        // pass A (count + date list) and pass B (Top hits candidates): one pass on the pause query;
        // on the typing path an unscored early-terminated date pass, then a scored one
        let passes = |e: &Self, query: &dyn TQuery| -> SResult<(Fruit, Option<Fruit>)> {
            if q.typing {
                let dcap = if q.before_key == 0 && need_date {
                    date_cap
                } else {
                    0
                };
                let fa = if !q.skip_list || !need_rank {
                    Some(e.run(&searcher, query, &mk(dcap, false, None, true, key_cap))?)
                } else {
                    None
                };
                let fb = if need_rank {
                    Some(e.run(
                        &searcher,
                        query,
                        &mk(rank_cap, true, extra.clone(), true, 0),
                    )?)
                } else {
                    None
                };
                match (fa, fb) {
                    (Some(a), b) => Ok((a, b)),
                    (None, Some(b)) => Ok((b, None)),
                    (None, None) => Ok((Fruit::default(), None)),
                }
            } else {
                let cap = if need_rank {
                    rank_cap.max(date_cap)
                } else {
                    date_cap
                };
                let f = e.run(
                    &searcher,
                    query,
                    &mk(cap, need_rank, extra.clone(), false, 0),
                )?;
                Ok((f, None))
            }
        };
        let tc = Instant::now();
        let (mut fa, mut fb) = passes(self, &*query)?;
        info.push(("collect_ms", ms(tc)));
        // typo trigger 2: fewer than 5 results (pause/Enter only)
        if !q.typing && o.typo && fa.distinct < 5 {
            let mut changed = false;
            for c in cls.iter_mut().filter(|c| !c.negate) {
                for t in c.terms.iter_mut().filter(|t| !t.partial) {
                    if let TK::Tiers {
                        t: w,
                        typo,
                        expand: true,
                        ..
                    } = &mut t.tk
                        && !*typo
                        && eligible(w)
                    {
                        *typo = true;
                        n_typo += 1;
                        changed = true;
                    }
                }
            }
            if changed && let Some(q2) = self.build(&cls, ALL, &fl, o, None) {
                query = q2;
                let t2 = Instant::now();
                (fa, fb) = passes(self, &*query)?;
                info.push(("typo2_ms", ms(t2)));
            }
        }
        let count = fa.distinct.min(o.count_cap) as u64;
        let count_capped = fa.distinct > o.count_cap;
        let fr = fb.as_ref().unwrap_or(&fa);
        // people-first resolution: fuzzy person matches add their messages as candidates
        let mut pset: HashSet<u64> = HashSet::new();
        let mut extra_people: Vec<Cand> = Vec::new();
        if o.people_first && self.people.is_some() {
            let tp = Instant::now();
            let mut fuzzy: Vec<((usize, usize), Vec<u64>)> = Vec::new();
            for (ci, c) in cls.iter().enumerate() {
                if c.negate || c.group != 0 {
                    continue;
                }
                for (ti, t) in c.terms.iter().enumerate() {
                    if t.fields & (F_FROM | F_TO) == 0 {
                        continue;
                    }
                    let TK::Tiers { t: w, .. } = &t.tk else {
                        continue;
                    };
                    if w.chars().count() < 3 || !w.chars().all(|c| c.is_alphabetic()) {
                        continue;
                    }
                    let (addrs, kind) = self.resolve_person(w, t.partial, &aff)?;
                    if kind == 3 {
                        fuzzy.push(((ci, ti), addrs.clone()));
                    }
                    pset.extend(addrs);
                }
            }
            let tf = Instant::now();
            for (skip, addrs) in fuzzy {
                let rest = Self::without(&cls, skip);
                if let Some(q2) = self.build(&rest, ALL, &fl, o, Some(self.sender_filter(&addrs))) {
                    let f2 = self.run(&searcher, &*q2, &mk(o.k_rank, need_rank, None, false, 0))?;
                    extra_people.extend(f2.newest);
                }
            }
            info.push(("pfuzzy_ms", ms(tf)));
            info.push(("people_ms", ms(tp)));
        }
        let mut hits = Hits {
            count,
            count_capped,
            ..Default::default()
        };
        // date list (copies collapse by duplicate key, newest kept)
        let td = Instant::now();
        let mut date_c: Vec<Cand> = Vec::new();
        if need_date {
            let page;
            let paged = if q.before_key != 0 {
                let mut fp = fl;
                let hi = q.before_key.max(0) as u64;
                fp.skey = Some(match fp.skey {
                    Some((lo, h)) => (lo, h.min(hi)),
                    None => (0, hi),
                });
                self.build(&cls, ALL, &fp, o, None)
            } else {
                None
            };
            let newest: &[Cand] = if let Some(qp) = paged {
                page = self.run(&searcher, &*qp, &mk(date_cap, false, None, true, 0))?;
                &page.newest
            } else {
                &fa.newest
            };
            let mut dl: Vec<Cand> = newest.to_vec();
            if !extra_people.is_empty() {
                let have: HashSet<u64> = dl.iter().map(|c| c.mid).collect();
                dl.extend(
                    extra_people
                        .iter()
                        .filter(|c| {
                            !have.contains(&c.mid)
                                && (q.before_key == 0 || c.skey < q.before_key as u64)
                        })
                        .copied(),
                );
                dl.sort_unstable_by(|a, b| b.skey.cmp(&a.skey).then(b.mid.cmp(&a.mid)));
            }
            let (rows, kept, folded) =
                collapse(dl.iter().map(|c| (c, 0f64)), q.collapse, q.n_list, o.folded);
            hits.date = rows;
            hits.folded.date = folded;
            date_c = kept;
        }
        info.push(("date_ms", ms(td)));
        // ranking
        let mut top_c: Vec<Cand> = Vec::new();
        let mut rel_c: Vec<Cand> = Vec::new();
        if need_rank && (fr.count > 0 || !extra_people.is_empty()) {
            let tr = Instant::now();
            let all = fr.count <= rank_cap && !fr.skipped;
            hits.from_recent = !all;
            let mut cands: Vec<Cand> = if all {
                fr.newest.clone()
            } else {
                fr.newest.iter().chain(fr.extra.iter()).copied().collect()
            };
            let mut have: HashSet<u64> = HashSet::with_capacity(cands.len());
            cands.retain(|c| have.insert(c.mid));
            for c in &extra_people {
                if have.insert(c.mid) {
                    cands.push(*c);
                }
            }
            let want_top = !q.skip_top && q.n_top > 0;
            let qx = if want_top {
                self.quoted_fallback(&searcher, &cls, &fl, o, q, &bits, &cands, &mut info)?
            } else {
                vec![0f32; cands.len()]
            };
            let f = &o.feat;
            let maxb = cands
                .iter()
                .enumerate()
                .map(|(k, c)| c.score as f64 + qx[k] as f64)
                .fold(f64::MIN, f64::max)
                .max(1e-9);
            let maxb0 = cands
                .iter()
                .map(|c| c.score as f64)
                .fold(f64::MIN, f64::max)
                .max(1e-9);
            let feats: Vec<[f64; 6]> = cands
                .iter()
                .map(|c| {
                    let received = (c.skey >> 20) as i64;
                    let age = ((q.now - received) as f64 / 86400.0).max(0.0);
                    let r = (-age / f.half_life_days).exp2();
                    let a = aff.a.get(&c.from_h).copied().unwrap_or(0.0);
                    let ow = if self.owner.contains(&c.from_h) {
                        1.0
                    } else {
                        0.0
                    };
                    let l = if c.bulk && a == 0.0 { 1.0 } else { 0.0 };
                    let p = if pset.contains(&c.from_h) { 1.0 } else { 0.0 };
                    let d = d_feature(received, q.hint);
                    [f.wr * r, f.wa * a, f.wo * ow, f.wd * d, f.wb * l, f.wp * p]
                })
                .collect();
            let rest = |x: &[f64; 6]| x[1] + x[2] + x[3] - x[4] + x[5];
            if want_top {
                let sc: Vec<(f64, usize)> = feats
                    .iter()
                    .enumerate()
                    .map(|(k, x)| {
                        let b = f.wt * ((cands[k].score as f64 + qx[k] as f64) / maxb);
                        (b + x[0] + rest(x), k)
                    })
                    .collect();
                // the reader returns messages: Top hits collapse copies only (conversations are
                // grouped by the caller's conversation view)
                let (rows, picked, folded) = rank(&cands, sc, q.collapse, q.n_top, o.folded);
                hits.top = rows;
                hits.folded.top = folded;
                top_c = picked;
            }
            if rel_wanted {
                let sc: Vec<(f64, usize)> = feats
                    .iter()
                    .enumerate()
                    .map(|(k, x)| (f.wt * (cands[k].score as f64 / maxb0) + rest(x), k))
                    .collect();
                let (rows, picked, folded) = rank(&cands, sc, q.collapse, q.n_list, o.folded);
                hits.rel = rows;
                hits.folded.rel = folded;
                rel_c = picked;
            }
            info.push(("rank_ms", ms(tr)));
        }
        // tiers, fields, labels (pause/Enter only)
        if !q.typing {
            let ta = Instant::now();
            let mut addrs: Vec<DocAddress> = top_c.iter().map(|c| c.addr).collect();
            addrs.extend(rel_c.iter().map(|c| c.addr));
            addrs.extend(date_c.iter().map(|c| c.addr));
            let ann = self.annotate(&searcher, &cls, o, &addrs)?;
            info.push(("bits_ms", ms(ta)));
            info.push(("ann_addrs", addrs.len() as f64));
            let (top_n, rel_n) = (top_c.len(), rel_c.len());
            for (k, h) in hits.top.iter_mut().enumerate() {
                (h.tiers, h.fields) = ann[k];
            }
            for (k, h) in hits.rel.iter_mut().enumerate() {
                (h.tiers, h.fields) = ann[top_n + k];
            }
            for (k, h) in hits.date.iter_mut().enumerate() {
                (h.tiers, h.fields) = ann[top_n + rel_n + k];
            }
            // tier labels: matched without stems/typos (0), with stems (1), only with typos (2)
            hits.labels = vec![0; hits.top.len()];
            if n_typo > 0 || n_stem > 0 {
                let strict = self.build(
                    &cls,
                    Tiers {
                        stem: false,
                        typo: false,
                        ..ALL
                    },
                    &fl,
                    o,
                    None,
                );
                let nontypo = self.build(&cls, Tiers { typo: false, ..ALL }, &fl, o, None);
                if let (Some(s), Some(nt)) = (strict, nontypo) {
                    let ws = s.weight(EnableScoring::disabled_from_searcher(&searcher))?;
                    let wn = nt.weight(EnableScoring::disabled_from_searcher(&searcher))?;
                    let ms_ = Self::matches(&searcher, &*ws, &addrs)?;
                    let mn = Self::matches(&searcher, &*wn, &addrs)?;
                    for (k, l) in hits.labels.iter_mut().enumerate() {
                        *l = if ms_[k] {
                            0
                        } else if mn[k] {
                            1
                        } else {
                            2
                        };
                    }
                    hits.corrected = n_typo > 0 && mn.iter().any(|m| !m);
                }
            }
            info.push(("annotate_ms", ms(ta)));
        }
        info.push(("total_ms", ms(t_all)));
        info.push(("n_stem", n_stem as f64));
        info.push(("n_typo", n_typo as f64));
        info.push(("people_addrs", pset.len() as f64));
        info.push(("segments", searcher.segment_readers().len() as f64));
        hits.info = info;
        Ok(hits)
    }

    /// Byte spans of `text` the query's positive clauses match (same normalisation and tokenizers
    /// as indexing), sorted, non-overlapping.
    pub fn highlight(&mut self, q: &Query, o: &Opts, text: &str) -> Vec<(u32, u32, u32)> {
        let cls = self.compile(q, o);
        crate::highlight::spans(&mut self.an, &self.snow, &cls_view(&cls), q.typing, text)
    }

    // ---- oracle ----

    /// Evaluation record (quality only, untimed): the complete matched set under
    /// this engine's rules (no cap; people candidates as the engine adds them), the target's BM25
    /// position and ties, exact-term-overlap ambiguity. `targets` are message ids.
    pub fn oracle(&mut self, q: &Query, o: &Opts, targets: &HashSet<u64>) -> SResult<Oracle> {
        self.cancel.store(false, Ordering::Relaxed);
        let searcher = self.mail.reader.searcher();
        let (mut cls, n_stem, mut n_typo) = self.prepare(q, o, &searcher);
        let fl = Filters {
            skey: q
                .date
                .map(|(lo, hi)| ((lo.max(0) as u64) << 20, (hi.max(0) as u64) << 20)),
            account: q.account,
        };
        let bits = if q.exclude && self.excluded.is_some() {
            self.ensure_bits(&searcher);
            Some(self.bits.clone())
        } else {
            None
        };
        let all = Coll {
            cap: usize::MAX / 2,
            extra: None,
            scoring: true,
            bits: bits.clone(),
            collapse: false,
            key_cap: 0,
            early: false,
            count_need: 0,
        };
        let Some(mut query) = self.build(&cls, ALL, &fl, o, None) else {
            return Ok(Oracle {
                unsat: true,
                ..Default::default()
            });
        };
        let mut fr = self.run(&searcher, &*query, &all)?;
        if o.typo && fr.count < 5 {
            let mut changed = false;
            for c in cls.iter_mut().filter(|c| !c.negate) {
                for t in c.terms.iter_mut().filter(|t| !t.partial) {
                    if let TK::Tiers {
                        t: w,
                        typo,
                        expand: true,
                        ..
                    } = &mut t.tk
                        && !*typo
                        && eligible(w)
                        && !q.typing
                    {
                        *typo = true;
                        n_typo += 1;
                        changed = true;
                    }
                }
            }
            if changed && let Some(q2) = self.build(&cls, ALL, &fl, o, None) {
                query = q2;
                fr = self.run(&searcher, &*query, &all)?;
            }
        }
        let mut info = Vec::new();
        let qx = self.quoted_fallback(&searcher, &cls, &fl, o, q, &bits, &fr.newest, &mut info)?;
        let mut m: HashMap<u64, f64> = fr
            .newest
            .iter()
            .enumerate()
            .map(|(k, c)| (c.mid, c.score as f64 + qx[k] as f64))
            .collect();
        let mut skeys: HashMap<u64, u64> = fr.newest.iter().map(|c| (c.mid, c.skey)).collect();
        let n_match = m.len();
        let hit = |m: &HashMap<u64, f64>| targets.iter().any(|t| m.contains_key(t));
        let mut via = if hit(&m) { "match" } else { "" };
        let mut pset: HashSet<u64> = HashSet::new();
        if o.people_first && self.people.is_some() {
            let aff = self.affinity();
            for (ci, c) in cls.iter().enumerate() {
                if c.negate || c.group != 0 {
                    continue;
                }
                for (ti, t) in c.terms.iter().enumerate() {
                    if t.fields & (F_FROM | F_TO) == 0 {
                        continue;
                    }
                    let TK::Tiers { t: w, .. } = &t.tk else {
                        continue;
                    };
                    if w.chars().count() < 3 || !w.chars().all(|c| c.is_alphabetic()) {
                        continue;
                    }
                    let (addrs, kind) = self.resolve_person(w, t.partial, &aff)?;
                    pset.extend(addrs.iter().copied());
                    if kind == 3 {
                        let rest = Self::without(&cls, (ci, ti));
                        if let Some(q2) =
                            self.build(&rest, ALL, &fl, o, Some(self.sender_filter(&addrs)))
                        {
                            for c in self.run(&searcher, &*q2, &all)?.newest {
                                m.entry(c.mid).or_insert(c.score as f64);
                                skeys.entry(c.mid).or_insert(c.skey);
                            }
                        }
                    }
                }
            }
            if via.is_empty() && hit(&m) {
                via = "people";
            }
        }
        let unscored = Coll {
            scoring: false,
            ..all
        };
        let mut sig: HashMap<u64, u64> = HashMap::new();
        let mut terms = Vec::new();
        let flat: Vec<(&CClause, &T)> = cls
            .iter()
            .filter(|c| !c.negate)
            .flat_map(|c| c.terms.iter().map(move |t| (c, t)))
            .collect();
        for (i, (c, t)) in flat.iter().enumerate().take(64) {
            let one = [CClause {
                terms: vec![(*t).clone()],
                negate: false,
                group: c.group,
            }];
            let mut ex_n = 0usize;
            let mut tgt_exact = false;
            if let Some(eq) = self.build(&one, EXACT, &fl, o, None) {
                for cand in self.run(&searcher, &*eq, &unscored)?.newest {
                    ex_n += 1;
                    if targets.contains(&cand.mid) {
                        tgt_exact = true;
                    }
                    if m.contains_key(&cand.mid) || targets.contains(&cand.mid) {
                        *sig.entry(cand.mid).or_default() |= 1u64 << i;
                    }
                }
            }
            let mut tgt_any = false;
            if let Some(aq) = self.build(&one, ALL, &fl, o, None) {
                let tq =
                    TermSetQuery::new(targets.iter().map(|t| Term::from_field_u64(self.f.mid, *t)));
                let both = BooleanQuery::new(vec![(Occur::Must, aq), (Occur::Must, Box::new(tq))]);
                tgt_any = self.run(&searcher, &both, &unscored)?.count > 0;
            }
            let text = match &t.tk {
                TK::Tiers { t, .. } => t.clone(),
                TK::Phrase { parts, .. } | TK::Near { parts, .. } => parts.join(" "),
                TK::Cjk { bigrams } => bigrams.join(" "),
            };
            terms.push(OracleTerm {
                t: text,
                exact_df: ex_n,
                target_exact: tgt_exact,
                target_any: tgt_any,
            });
        }
        let mut rec = Oracle {
            unsat: false,
            n_terms: flat.len(),
            count_est: fr.count,
            n_match,
            n_all: m.len(),
            retrievable: !via.is_empty(),
            via,
            terms,
            n_stem,
            n_typo,
            people_addrs: pset.len(),
            ranks: None,
        };
        if !via.is_empty() {
            let ts = targets
                .iter()
                .filter_map(|t| m.get(t))
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max);
            let tsig = targets
                .iter()
                .map(|t| sig.get(t).copied().unwrap_or(0))
                .max_by_key(|s| s.count_ones())
                .unwrap_or(0);
            let tmax = targets
                .iter()
                .filter(|t| m.contains_key(t))
                .filter_map(|t| skeys.get(t).copied())
                .max()
                .unwrap_or(0);
            let eps = 1e-6 * ts.abs().max(1.0);
            let mut r = OracleRanks {
                tsig_bits: tsig.count_ones(),
                ..Default::default()
            };
            for (id, s) in &m {
                if targets.contains(id) {
                    continue;
                }
                if *s > ts + eps {
                    r.bm_better += 1;
                } else if (*s - ts).abs() <= eps {
                    r.bm_ties += 1;
                }
                let sd = sig.get(id).copied().unwrap_or(0);
                if sd == tsig {
                    r.ov_ties += 1;
                } else if sd & tsig == tsig {
                    r.ov_better += 1;
                }
                if skeys.get(id).copied().unwrap_or(0) > tmax {
                    r.date_better += 1;
                }
            }
            rec.ranks = Some(r);
        }
        Ok(rec)
    }
}

/// D: 1 inside the hint range, linear decay to 0 over one range width outside.
fn d_feature(date: i64, hint: Option<(i64, i64)>) -> f64 {
    match hint {
        None => 0.0,
        Some((lo, hi)) => {
            if date >= lo && date < hi {
                1.0
            } else {
                let w = (hi - lo).max(86400) as f64;
                let dist = if date < lo {
                    (lo - date) as f64
                } else {
                    (date - hi) as f64
                };
                (1.0 - dist / w).max(0.0)
            }
        }
    }
}

/// Keeps the first row of every group of copies (rows in final order; a copy shares the
/// Message-ID hash or the body hash with a kept row), counting the folded copies; at most `k`
/// rows. Returns the rows, the kept candidates and, when `record`, the folded copies as (row,
/// skey).
fn collapse<'a>(
    rows: impl Iterator<Item = (&'a Cand, f64)>,
    on: bool,
    k: usize,
    record: bool,
) -> (Vec<Hit>, Vec<Cand>, Vec<(u32, u64)>) {
    let mut folded: Vec<(u32, u64)> = Vec::new();
    let mut out: Vec<Hit> = Vec::new();
    let mut kept: Vec<Cand> = Vec::new();
    let mut by_mid: HashMap<u64, usize> = HashMap::new();
    let mut by_body: HashMap<u64, usize> = HashMap::new();
    for (c, s) in rows {
        if on {
            let found = (c.dup_mid != 0)
                .then(|| by_mid.get(&c.dup_mid))
                .flatten()
                .or_else(|| {
                    (c.dup_body != 0)
                        .then(|| by_body.get(&c.dup_body))
                        .flatten()
                })
                .copied();
            let row = match found {
                Some(i) => {
                    out[i].copies += 1;
                    if record {
                        folded.push((i as u32, c.skey));
                    }
                    i
                }
                None if out.len() >= k => continue,
                None => out.len(),
            };
            if c.dup_mid != 0 {
                by_mid.entry(c.dup_mid).or_insert(row);
            }
            if c.dup_body != 0 {
                by_body.entry(c.dup_body).or_insert(row);
            }
            if found.is_some() {
                continue;
            }
        } else if out.len() >= k {
            break;
        }
        out.push(Hit {
            mid: c.mid,
            skey: c.skey,
            score: s as f32,
            ..Default::default()
        });
        kept.push(*c);
    }
    (out, kept, folded)
}

/// The `k` best of `sc` (score, candidate index), copies collapsed: twice the window first, the
/// whole list only if that leaves fewer than `k` distinct rows .
fn rank(
    cands: &[Cand],
    sc: Vec<(f64, usize)>,
    on: bool,
    k: usize,
    record: bool,
) -> (Vec<Hit>, Vec<Cand>, Vec<(u32, u64)>) {
    let ord = |a: &(f64, usize), b: &(f64, usize)| {
        b.0.total_cmp(&a.0)
            .then(cands[b.1].skey.cmp(&cands[a.1].skey))
            .then(cands[b.1].mid.cmp(&cands[a.1].mid))
    };
    let want = k.min(sc.len());
    if want == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let window = if on { (want * 2).min(sc.len()) } else { want };
    let pick = |w: usize| {
        let mut v = sc.clone();
        if w < v.len() {
            v.select_nth_unstable_by(w - 1, ord);
            v.truncate(w);
        }
        v.sort_by(ord);
        collapse(v.iter().map(|x| (&cands[x.1], x.0)), on, want, record)
    };
    let r = pick(window);
    if on && r.0.len() < want && sc.len() > window {
        return pick(sc.len());
    }
    r
}

/// Compiled clause view for highlighting (crate-internal).
pub(crate) enum HlTerm<'a> {
    Word {
        t: &'a str,
        prefix: bool,
        stem: bool,
    },
    Phrase {
        parts: &'a [String],
        last_prefix: bool,
    },
    Cjk {
        bigrams: &'a [String],
    },
}

fn cls_view(cls: &[CClause]) -> Vec<HlTerm<'_>> {
    cls.iter()
        .filter(|c| !c.negate)
        .flat_map(|c| c.terms.iter())
        .flat_map(|t| match &t.tk {
            TK::Tiers {
                t: w,
                prefix,
                expand,
                ..
            } => vec![HlTerm::Word {
                t: w,
                prefix: *prefix,
                stem: *expand && !t.partial && eligible(w),
            }],
            TK::Phrase { parts, last_prefix } => vec![HlTerm::Phrase {
                parts,
                last_prefix: *last_prefix,
            }],
            // AROUND: each word where it occurs (the words need not be adjacent)
            TK::Near { parts, .. } => parts
                .iter()
                .map(|p| HlTerm::Word {
                    t: p,
                    prefix: false,
                    stem: false,
                })
                .collect(),
            TK::Cjk { bigrams } => vec![HlTerm::Cjk { bigrams }],
        })
        .collect()
}

/// The search-terms part of an [`Oracle`] record.
#[derive(Clone, Debug, Default)]
pub struct OracleTerm {
    pub t: String,
    pub exact_df: usize,
    pub target_exact: bool,
    pub target_any: bool,
}

/// Rank evidence of a retrievable target.
#[derive(Clone, Debug, Default)]
pub struct OracleRanks {
    pub bm_better: usize,
    pub bm_ties: usize,
    pub ov_better: usize,
    pub ov_ties: usize,
    pub date_better: usize,
    pub tsig_bits: u32,
}

/// The per-query evidence record.
#[derive(Clone, Debug, Default)]
pub struct Oracle {
    pub unsat: bool,
    pub n_terms: usize,
    pub count_est: usize,
    pub n_match: usize,
    pub n_all: usize,
    pub retrievable: bool,
    pub via: &'static str,
    pub terms: Vec<OracleTerm>,
    pub n_stem: usize,
    pub n_typo: usize,
    pub people_addrs: usize,
    pub ranks: Option<OracleRanks>,
}

impl Oracle {
    /// The JSON line (`qid`, `class` supplied by the bench).
    pub fn to_json(&self, qid: &str, class: &str) -> String {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        if self.unsat {
            return format!(
                "{{\"qid\":\"{}\",\"class\":\"{}\",\"unsat\":true,\"retrievable\":false,\"via\":\"\"}}",
                esc(qid),
                esc(class)
            );
        }
        let terms: Vec<String> = self
            .terms
            .iter()
            .map(|t| {
                format!(
                    "{{\"t\":\"{}\",\"exact_df\":{},\"target_exact\":{},\"target_any\":{}}}",
                    esc(&t.t),
                    t.exact_df,
                    t.target_exact,
                    t.target_any
                )
            })
            .collect();
        let mut rec = format!(
            "{{\"qid\":\"{}\",\"class\":\"{}\",\"n_terms\":{},\"count_est\":{},\"n_match\":{},\"n_all\":{},\"retrievable\":{},\"via\":\"{}\",\"terms\":[{}],\"n_stem\":{},\"n_typo\":{},\"people_addrs\":{}",
            esc(qid),
            esc(class),
            self.n_terms,
            self.count_est,
            self.n_match,
            self.n_all,
            self.retrievable,
            self.via,
            terms.join(","),
            self.n_stem,
            self.n_typo,
            self.people_addrs
        );
        if let Some(r) = &self.ranks {
            rec += &format!(
                ",\"bm_better\":{},\"bm_ties\":{},\"ov_better\":{},\"ov_ties\":{},\"date_better\":{},\"tsig_bits\":{}",
                r.bm_better, r.bm_ties, r.ov_better, r.ov_ties, r.date_better, r.tsig_bits
            );
        }
        rec.push('}');
        rec
    }
}

/// All live message ids (tests, verification).
pub fn all_mids(e: &Engine) -> Vec<u64> {
    let s = e.mail.reader.searcher();
    let coll = Coll {
        cap: usize::MAX / 2,
        extra: None,
        scoring: false,
        bits: None,
        collapse: false,
        key_cap: 0,
        early: false,
        count_need: 0,
    };
    e.run(&s, &AllQuery, &coll)
        .map(|f| f.newest.iter().map(|c| c.mid).collect())
        .unwrap_or_default()
}

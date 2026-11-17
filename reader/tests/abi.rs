//! The C ABI over a small index written here with Tantivy and mailsearch-core's layout (no
//! dependency on the indexer crate).

use mailsearch::engine::{Engine, Opts, Qfb, Query};
use mailsearch::*;
use mailsearch_core::{
    CorrInput, DocInput, Fields, PeopleFields, PersonInput, corr_doc, people_schema, person_doc,
    register, schema, stamp, to_tantivy,
};
use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const T0: i64 = 1_700_000_000;
const DAY: i64 = 86_400;

fn skey(day: i64, slot: i64) -> i64 {
    ((T0 + day * DAY) << 20) + slot
}

fn now() -> i64 {
    T0 + 400 * DAY
}

struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

static N: AtomicU32 = AtomicU32::new(0);

fn doc(mid: u64, day: i64, subject: &str, body: &str) -> DocInput {
    DocInput {
        mid,
        search_key: skey(day, mid as i64),
        subject: subject.into(),
        body: body.into(),
        from: "Carol <carol@example.org>".into(),
        from_h: mailsearch_core::fnv1a64(b"carol@example.org"),
        account: 1,
        ..Default::default()
    }
}

fn corpus() -> Vec<DocInput> {
    let alice = mailsearch_core::fnv1a64(b"alice@example.com");
    vec![
        DocInput {
            from: "Alice Smith <alice@example.com>".into(),
            from_h: alice,
            conv: 10,
            ..doc(1, 0, "Invoice for March", "Please pay the invoice")
        },
        DocInput {
            conv: 11,
            ..doc(2, 300, "Invoice reminder", "the invoice is overdue")
        },
        doc(3, 100, "Lunch", "sandwiches"),
        DocInput {
            quoted: "about the zephyr project plan".into(),
            conv: 20,
            ..doc(4, 50, "Re: plans", "thanks")
        },
        DocInput {
            quoted: "about the zephyr project plan".into(),
            conv: 21,
            ..doc(6, 50, "Re: plans", "thanks")
        },
        DocInput {
            conv: 21,
            ..doc(7, 10, "Plans", "the zephyr project")
        },
        DocInput {
            msgid_h: 77,
            ..doc(8, 200, "Duplicate notice", "copy")
        },
        DocInput {
            msgid_h: 77,
            body_h: 500,
            ..doc(9, 201, "Duplicate notice", "copy")
        },
        DocInput {
            body_h: 500,
            ..doc(17, 202, "Duplicate notice", "copy")
        },
        DocInput {
            conv: 30,
            ..doc(10, 210, "Budget review", "numbers attached")
        },
        DocInput {
            conv: 30,
            ..doc(11, 211, "Re: Budget review", "looks fine")
        },
        doc(12, 5, "会議室の予約", "room"),
        doc(13, 0, "kangaroo", "old"),
        doc(14, 380, "kangaroo", "new"),
        DocInput {
            account: 2,
            ..doc(15, 390, "Invoice other account", "invoice")
        },
        doc(16, 20, "Phone", "call 555-123-4567 today"),
    ]
}

fn write_index(root: &Path, n: u32, docs: &[DocInput], with_people: bool) {
    let idx = root.join(format!("idx-{n}"));
    let mail = idx.join("mail");
    std::fs::create_dir_all(&mail).unwrap();
    let index = tantivy::Index::create_in_dir(&mail, schema()).unwrap();
    register(&index);
    let f = Fields::new(&index.schema()).unwrap();
    let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
    for d in docs {
        w.add_document(to_tantivy(&f, d)).unwrap();
    }
    w.commit().unwrap();
    if with_people {
        let people = idx.join("people");
        std::fs::create_dir_all(&people).unwrap();
        let pi = tantivy::Index::create_in_dir(&people, people_schema()).unwrap();
        register(&pi);
        let pf = PeopleFields::new(&pi.schema()).unwrap();
        let mut pw: tantivy::IndexWriter = pi.writer_with_num_threads(1, 15_000_000).unwrap();
        for (id, addr, name, sent) in [
            (100, "alice@example.com", "Alice Smith", 10),
            (101, "bob@example.net", "Bob Jones", 1),
            (102, "alina@example.org", "Alina Brown", 0),
        ] {
            pw.add_document(person_doc(
                &pf,
                &PersonInput {
                    id,
                    account: 1,
                    addr_norm: addr.into(),
                    names: vec![name.into()],
                },
            ))
            .unwrap();
            pw.add_document(corr_doc(
                &pf,
                &CorrInput {
                    id,
                    sent_to: sent,
                    received_from: 3,
                    ..Default::default()
                },
            ))
            .unwrap();
        }
        pw.commit().unwrap();
    }
    std::fs::write(idx.join("VERSION"), stamp()).unwrap();
    std::fs::write(root.join("CURRENT"), format!("idx-{n}\n")).unwrap();
}

fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "mailsearch-abi-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    write_index(&root, 1, &corpus(), true);
    Fixture { root }
}

fn open(root: &Path) -> Result<*mut MailsearchHandle, i32> {
    let r = root.to_str().unwrap();
    let mut err = 99;
    // SAFETY: valid pointers.
    let g = unsafe { mailsearch_open(r.as_ptr() as *const c_char, r.len(), &mut err) };
    if g.is_null() { Err(err) } else { Ok(g) }
}

struct C {
    text: Vec<u8>,
    fields: u32,
    mode: u32,
    flags: u32,
    group: u32,
    near: u32,
}

fn c(text: &str) -> C {
    C {
        text: text.as_bytes().to_vec(),
        fields: engine::F_TEXT,
        mode: 0,
        flags: 0,
        group: 0,
        near: 0,
    }
}

struct Out {
    rc: i32,
    top: Vec<MailsearchHit>,
    list: Vec<MailsearchHit>,
    count: u64,
    capped: u32,
    flags: u32,
}

fn mids(v: &[MailsearchHit]) -> Vec<u64> {
    v.iter().map(|h| h.mid).collect()
}

#[derive(Clone, Copy, Default)]
struct Q {
    flags: u32,
    n_top: u32,
    date: (i64, i64),
    hint: (i64, i64),
    before_key: i64,
    account: u64,
}

fn search(g: *mut MailsearchHandle, cls: &[C], q: Q) -> Out {
    let gc: Vec<MailsearchClause> = cls
        .iter()
        .map(|c| MailsearchClause {
            text: c.text.as_ptr() as *const c_char,
            len: c.text.len(),
            fields: c.fields,
            mode: c.mode,
            flags: c.flags,
            group: c.group,
            near: c.near,
        })
        .collect();
    let gq = MailsearchQuery {
        clauses: gc.as_ptr(),
        n_clauses: gc.len(),
        now: now(),
        date_lo: q.date.0,
        date_hi: q.date.1,
        hint_lo: q.hint.0,
        hint_hi: q.hint.1,
        before_key: q.before_key,
        account: q.account,
        flags: q.flags,
        n_top: q.n_top,
    };
    let mut top = vec![MailsearchHit::default(); 7];
    let mut list = vec![MailsearchHit::default(); 50];
    let mut r = MailsearchResult {
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
    // SAFETY: all pointers valid for the call.
    let rc = unsafe { mailsearch_search(g, &gq, &mut r) };
    top.truncate(r.n_top);
    list.truncate(r.n_list);
    Out {
        rc,
        top,
        list,
        count: r.count,
        capped: r.count_capped,
        flags: r.flags,
    }
}

fn q5() -> Q {
    Q {
        n_top: 5,
        ..Default::default()
    }
}

fn close(g: *mut MailsearchHandle) {
    // SAFETY: live handle.
    assert_eq!(unsafe { mailsearch_close(g) }, MAILSEARCH_OK);
}

fn last_error() -> String {
    let mut buf = vec![0u8; 256];
    // SAFETY: valid buffer.
    let n = unsafe { mailsearch_last_error(buf.as_mut_ptr() as *mut c_char, buf.len()) };
    String::from_utf8_lossy(&buf[..n.min(255)]).into_owned()
}

#[test]
fn shared_vectors() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/vectors");
    let n = mailsearch_core::vectors::check_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    assert!(n > 0);
}

#[test]
fn version_refusal_and_missing_index() {
    let fx = fixture();
    let v = fx.root.join("idx-1/VERSION");
    std::fs::remove_file(&v).unwrap();
    assert_eq!(open(&fx.root).unwrap_err(), MAILSEARCH_E_VERSION);
    std::fs::write(&v, "{not json").unwrap();
    assert_eq!(open(&fx.root).unwrap_err(), MAILSEARCH_E_VERSION);
    std::fs::write(
        &v,
        stamp().replace("\"layout_version\":2", "\"layout_version\":1"),
    )
    .unwrap();
    assert_eq!(open(&fx.root).unwrap_err(), MAILSEARCH_E_VERSION);
    assert!(last_error().contains("layout_version"));
    std::fs::write(&v, stamp().replace("0.27.0", "0.26.0")).unwrap();
    assert_eq!(open(&fx.root).unwrap_err(), MAILSEARCH_E_VERSION);
    std::fs::write(&v, stamp()).unwrap();
    let g = open(&fx.root).unwrap();
    close(g);
    std::fs::remove_file(fx.root.join("CURRENT")).unwrap();
    assert_eq!(open(&fx.root).unwrap_err(), MAILSEARCH_E_IO);
}

#[test]
fn null_and_bad_arguments() {
    let fx = fixture();
    let mut err = 0;
    // SAFETY: deliberately null / invalid arguments the ABI must refuse.
    unsafe {
        assert!(mailsearch_open(std::ptr::null(), 5, &mut err).is_null());
        assert_eq!(err, MAILSEARCH_E_ARG);
        let bad = [0xffu8, 0xfe];
        assert!(mailsearch_open(bad.as_ptr() as *const c_char, 2, &mut err).is_null());
        assert_eq!(err, MAILSEARCH_E_ARG);
        let g = open(&fx.root).unwrap();
        let mut r = MailsearchResult {
            top: std::ptr::null_mut(),
            top_cap: 0,
            n_top: 0,
            list: std::ptr::null_mut(),
            list_cap: 0,
            n_list: 0,
            count: 0,
            count_capped: 0,
            flags: 0,
        };
        assert_eq!(
            mailsearch_search(g, std::ptr::null(), &mut r),
            MAILSEARCH_E_ARG
        );
        assert_eq!(
            mailsearch_search(std::ptr::null_mut(), std::ptr::null(), &mut r),
            MAILSEARCH_E_ARG
        );
        let mut n = 0u64;
        assert_eq!(
            mailsearch_num_docs(std::ptr::null_mut(), &mut n),
            MAILSEARCH_E_ARG
        );
        assert_eq!(
            mailsearch_num_docs(g, std::ptr::null_mut()),
            MAILSEARCH_E_ARG
        );
        assert_eq!(
            mailsearch_people(
                g,
                "al".as_ptr() as *const c_char,
                2,
                std::ptr::null_mut(),
                3
            ),
            MAILSEARCH_E_ARG
        );
        assert_eq!(
            mailsearch_set_excluded(g, std::ptr::null(), 3),
            MAILSEARCH_E_ARG
        );
        assert_eq!(
            mailsearch_set_owner(g, std::ptr::null(), std::ptr::null(), 1),
            MAILSEARCH_E_ARG
        );
        let (mut d, mut lease_gen) = (0u32, 0u64);
        assert_eq!(
            mailsearch_reload(g, std::ptr::null_mut(), &mut lease_gen),
            MAILSEARCH_E_ARG
        );
        assert_eq!(mailsearch_reload(g, &mut d, &mut lease_gen), MAILSEARCH_OK);
        assert_eq!(
            mailsearch_release(g, std::ptr::null_mut()),
            MAILSEARCH_E_ARG
        );
        assert_eq!(mailsearch_cancel(std::ptr::null_mut()), MAILSEARCH_E_ARG);
        assert_eq!(mailsearch_close(std::ptr::null_mut()), MAILSEARCH_E_ARG);
        // bad flags, mode, fields, n_top, invalid UTF-8, too small capacity
        let mut bad_c = c("invoice");
        bad_c.mode = 9;
        assert_eq!(search(g, &[bad_c], q5()).rc, MAILSEARCH_E_ARG);
        let mut bad_f = c("invoice");
        bad_f.fields = 1 << 20;
        assert_eq!(search(g, &[bad_f], q5()).rc, MAILSEARCH_E_ARG);
        assert_eq!(
            search(
                g,
                &[c("invoice")],
                Q {
                    flags: 1 << 9,
                    ..q5()
                }
            )
            .rc,
            MAILSEARCH_E_ARG
        );
        assert_eq!(
            search(g, &[c("invoice")], Q { n_top: 8, ..q5() }).rc,
            MAILSEARCH_E_ARG
        );
        let invalid = C {
            text: vec![b'a', 0xff],
            ..c("")
        };
        assert_eq!(search(g, &[invalid], q5()).rc, MAILSEARCH_E_ARG);
        // no clauses: nothing matches
        let o = search(g, &[], q5());
        assert_eq!(
            (o.rc, o.count, o.top.len(), o.list.len()),
            (MAILSEARCH_OK, 0, 0, 0)
        );
        close(g);
    }
}

#[test]
fn top_hits_date_list_and_recency_only_in_top() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let o = search(g, &[c("invoice")], Q { n_top: 1, ..q5() });
    assert_eq!(o.rc, MAILSEARCH_OK);
    assert!(o.top.len() <= 1);
    assert_eq!(mids(&o.list), vec![15, 2, 1]);
    assert_eq!(o.count, 3);
    assert!(o.list.iter().all(|h| h.score == 0.0));
    assert!(o.top[0].tiers & engine::T_EXACT != 0);
    assert!(o.top[0].fields & engine::F_SUBJECT != 0);
    // account filter
    let o = search(g, &[c("invoice")], Q { account: 1, ..q5() });
    assert_eq!(mids(&o.list), vec![2, 1]);
    // recency: Top-hits score − relevance score of the same message = w_r · R, only in Top hits
    let top = search(g, &[c("kangaroo")], q5());
    let rel = search(
        g,
        &[c("kangaroo")],
        Q {
            flags: MAILSEARCH_Q_RELEVANCE,
            ..q5()
        },
    );
    assert_eq!(top.top.len(), 2);
    assert_eq!(rel.list.len(), 2);
    for h in &top.top {
        let r = rel.list.iter().find(|x| x.mid == h.mid).unwrap();
        let age = (now() - (h.search_key >> 20)) as f64 / DAY as f64;
        let want = 0.1 * (-age / 365.0).exp2();
        assert!(
            ((h.score - r.score) as f64 - want).abs() < 1e-4,
            "{h:?} {r:?}"
        );
    }
    // relevance list ordered by score
    assert!(rel.list.windows(2).all(|w| w[0].score >= w[1].score));
    close(g);
}

#[test]
fn excluded_set() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let ex = [2u64, 15];
    // SAFETY: valid array.
    assert_eq!(
        unsafe { mailsearch_set_excluded(g, ex.as_ptr(), ex.len()) },
        MAILSEARCH_OK
    );
    let o = search(g, &[c("invoice")], q5());
    assert_eq!((mids(&o.list), o.count), (vec![1], 1));
    assert_eq!(mids(&o.top), vec![1]);
    let o = search(
        g,
        &[c("invoice")],
        Q {
            flags: MAILSEARCH_Q_NO_EXCLUDE,
            ..q5()
        },
    );
    assert_eq!(o.count, 3);
    // SAFETY: empty set.
    assert_eq!(
        unsafe { mailsearch_set_excluded(g, std::ptr::null(), 0) },
        MAILSEARCH_OK
    );
    assert_eq!(search(g, &[c("invoice")], q5()).count, 3);
    close(g);
}

#[test]
fn date_paging_and_range() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let o = search(
        g,
        &[c("invoice")],
        Q {
            before_key: skey(300, 2),
            ..q5()
        },
    );
    assert_eq!(mids(&o.list), vec![1]);
    assert_eq!(o.count, 3, "paging does not change the count");
    let o = search(
        g,
        &[c("invoice")],
        Q {
            date: (T0 + 250 * DAY, T0 + 395 * DAY),
            ..q5()
        },
    );
    assert_eq!((mids(&o.list), o.count), (vec![15, 2], 2));
    close(g);
}

#[test]
fn collapse_copies_not_conversations() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    // 8 and 9 share the Message-ID hash, 9 and 17 the body hash: one row (either field matches)
    let o = search(g, &[c("duplicate")], q5());
    assert_eq!(mids(&o.list), vec![17]);
    assert_eq!(o.list[0].copies, 2);
    assert_eq!(o.count, 1);
    assert_eq!(mids(&o.top), vec![17]);
    assert_eq!(o.top[0].copies, 2);
    let o = search(
        g,
        &[c("duplicate")],
        Q {
            flags: MAILSEARCH_Q_NO_COLLAPSE,
            ..q5()
        },
    );
    assert_eq!((mids(&o.list), o.count), (vec![17, 9, 8], 3));
    // messages of one conversation are not copies: two rows in Top hits and in the list
    let o = search(g, &[c("budget")], q5());
    assert_eq!(o.top.len(), 2);
    assert!(o.top.iter().all(|h| h.copies == 0));
    assert_eq!(mids(&o.list), vec![11, 10]);
    close(g);
}

#[test]
fn folded_copies_per_row() {
    let fx = fixture();
    let mut e = Engine::open(&fx.root).unwrap();
    let q = Query {
        clauses: vec![engine::Clause {
            text: "duplicate".into(),
            fields: engine::F_TEXT,
            ..Default::default()
        }],
        now: now(),
        n_top: 5,
        n_list: 10,
        ..Default::default()
    };
    let h = e.search(&q, &Opts::default()).unwrap();
    assert!(h.folded.top.is_empty() && h.folded.date.is_empty() && h.folded.rel.is_empty());
    let all = e
        .search(
            &Query {
                collapse: false,
                ..q.clone()
            },
            &Opts::default(),
        )
        .unwrap();
    let mut copies: Vec<u64> = all.date.iter().map(|x| x.skey).collect();
    copies.sort_unstable();
    assert_eq!(copies.len(), 3);
    let o = Opts {
        folded: true,
        rel_list: true,
        ..Opts::default()
    };
    let h = e.search(&q, &o).unwrap();
    // 8, 9 and 17 are one row in every list; the other two are folded into it
    for (rows, folded) in [
        (&h.top, &h.folded.top),
        (&h.date, &h.folded.date),
        (&h.rel, &h.folded.rel),
    ] {
        assert_eq!(rows.len(), 1);
        assert!(folded.iter().all(|f| f.0 == 0), "{folded:?}");
        let mut v: Vec<u64> = folded.iter().map(|f| f.1).chain([rows[0].skey]).collect();
        v.sort_unstable();
        assert_eq!(v, copies);
    }
}

#[test]
fn quoted_fallback_in_top_hits_only() {
    let fx = fixture();
    let mut e = Engine::open(&fx.root).unwrap();
    let q = Query {
        clauses: vec![engine::Clause {
            text: "zephyr".into(),
            fields: engine::F_TEXT,
            ..Default::default()
        }],
        now: now(),
        n_top: 5,
        relevance: true,
        collapse: false,
        ..Default::default()
    };
    let o = Opts::default();
    let h = e.search(&q, &o).unwrap();
    let score = |v: &[engine::Hit], mid| v.iter().find(|x| x.mid == mid).unwrap().score;
    // 4: quoted only, alone in its conversation → fallback; 6: quoted only but 7 (same
    // conversation) has it in the body → no fallback
    assert!(score(&h.top, 4) > score(&h.top, 6) + 0.01, "{:?}", h.top);
    assert!(
        (score(&h.rel, 4) - score(&h.rel, 6)).abs() < 1e-6,
        "{:?}",
        h.rel
    );
    let off = e
        .search(
            &q,
            &Opts {
                qfb: Qfb::Off,
                ..o.clone()
            },
        )
        .unwrap();
    assert!((score(&off.top, 4) - score(&off.top, 6)).abs() < 1e-6);
    // while typing the word being typed gets no fallback
    let mut qt = q.clone();
    qt.typing = true;
    qt.clauses[0].partial = true;
    let t = e.search(&qt, &o).unwrap();
    assert!(
        (score(&t.top, 4) - score(&t.top, 6)).abs() < 1e-6,
        "{:?}",
        t.top
    );
}

#[test]
fn cjk_bigram_query_and_numbers() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    assert_eq!(mids(&search(g, &[c("会議")], q5()).list), vec![12]);
    assert_eq!(mids(&search(g, &[c("予約")], q5()).list), vec![12]);
    assert!(search(g, &[c("議予")], q5()).list.is_empty());
    assert_eq!(mids(&search(g, &[c("5551234567")], q5()).list), vec![16]);
    assert_eq!(mids(&search(g, &[c("555-123-4567")], q5()).list), vec![16]);
    close(g);
}

#[test]
fn typing_path_matches_pause_query_date_list() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let pause = search(g, &[c("invoice")], q5());
    let mut cl = c("invoi");
    cl.flags = MAILSEARCH_C_PARTIAL;
    let pass1 = search(
        g,
        &[cl],
        Q {
            flags: MAILSEARCH_Q_TYPING | MAILSEARCH_Q_SKIP_TOP,
            ..q5()
        },
    );
    assert_eq!(pass1.rc, MAILSEARCH_OK);
    assert!(pass1.top.is_empty());
    assert_eq!(mids(&pass1.list), mids(&pause.list));
    assert_eq!(pass1.count, pause.count);
    let mut cl = c("invoi");
    cl.flags = MAILSEARCH_C_PARTIAL;
    let pass2 = search(
        g,
        &[cl],
        Q {
            flags: MAILSEARCH_Q_TYPING | MAILSEARCH_Q_SKIP_LIST,
            ..q5()
        },
    );
    assert!(pass2.list.is_empty());
    assert_eq!(pass2.top.len(), 3);
    // short partial word: hpfx (subject/people words) + the typed word exactly anywhere
    let mut cl = c("bu");
    cl.flags = MAILSEARCH_C_PARTIAL;
    let o = search(
        g,
        &[cl],
        Q {
            flags: MAILSEARCH_Q_TYPING,
            ..q5()
        },
    );
    assert_eq!(mids(&o.list), vec![11, 10]);
    // a 1-letter word being typed feeds the People suggestions only
    let mut cl = c("b");
    cl.flags = MAILSEARCH_C_PARTIAL;
    assert_eq!(
        search(
            g,
            &[cl],
            Q {
                flags: MAILSEARCH_Q_TYPING,
                ..q5()
            }
        )
        .count,
        0
    );
    close(g);
}

#[test]
fn groups_negation_phrases_and_stems() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let mut a = c("lunch");
    a.group = 1;
    let mut b = c("budget");
    b.group = 1;
    assert_eq!(mids(&search(g, &[a, b], q5()).list), vec![11, 10, 3]);
    let mut n = c("reminder");
    n.flags = MAILSEARCH_C_NEGATE;
    assert_eq!(mids(&search(g, &[c("invoice"), n], q5()).list), vec![15, 1]);
    let mut p = c("pay the invoice");
    p.mode = 2;
    assert_eq!(mids(&search(g, &[p], q5()).list), vec![1]);
    let mut p = c("the pay invoice");
    p.mode = 2;
    assert!(search(g, &[p], q5()).list.is_empty());
    // AROUND (MAILSEARCH_M_NEAR): "Please pay the invoice" has two words between "please" and "invoice"
    let near = |text: &str, n: u32| {
        let mut p = c(text);
        p.mode = 4;
        p.near = n;
        mids(&search(g, &[p], q5()).list)
    };
    assert_eq!(near("please invoice", 2), vec![1]);
    assert_eq!(near("invoice please", 2), vec![1], "either order");
    assert!(near("please invoice", 1).is_empty());
    assert!(near("pleas invoice", 5).is_empty(), "exact tokens only");
    assert_eq!(near("invoice", 0), vec![15, 2, 1], "one word: that word");
    // identifier → phrase of parts
    assert_eq!(
        mids(&search(g, &[c("alice@example.com")], q5()).list),
        vec![1]
    );
    // stems: "invoices" finds "invoice" as a stem-tier match (pause/Enter)
    let o = search(g, &[c("invoices")], q5());
    assert_eq!(mids(&o.list), vec![15, 2, 1]);
    assert!(o.list.iter().all(|h| h.tiers & engine::T_STEM != 0));
    // exact mode: no prefix, no stems
    let mut x = c("invoices");
    x.mode = 1;
    assert!(search(g, &[x], q5()).list.is_empty());
    // typo tier: absent word, distance 1
    let o = search(g, &[c("invoise")], q5());
    assert_eq!(o.count, 3);
    assert!(o.flags & MAILSEARCH_R_CORRECTED != 0);
    assert!(o.top.iter().all(|h| h.tiers & engine::T_TYPO != 0));
    close(g);
}

#[test]
fn stem_languages() {
    let root = std::env::temp_dir().join(format!("mailsearch-abi-lang-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write_index(
        &root,
        1,
        &[doc(1, 0, "Rechnung", "die zahlung ist offen")],
        false,
    );
    let g = open(&root).unwrap();
    let set = |codes: &[&str]| {
        let p: Vec<*const c_char> = codes.iter().map(|c| c.as_ptr() as *const c_char).collect();
        let l: Vec<usize> = codes.iter().map(|c| c.len()).collect();
        // SAFETY: valid arrays.
        unsafe { mailsearch_set_languages(g, p.as_ptr(), l.as_ptr(), p.len()) }
    };
    // English only: "zahlungen" has no English stem match
    assert!(search(g, &[c("zahlungen")], q5()).list.is_empty());
    assert_eq!(set(&["de", "xx"]), MAILSEARCH_OK);
    let o = search(g, &[c("zahlungen")], q5());
    assert_eq!(mids(&o.list), vec![1]);
    assert!(o.list.iter().all(|h| h.tiers & engine::T_STEM != 0));
    assert_eq!(set(&[]), MAILSEARCH_OK);
    assert!(search(g, &[c("zahlungen")], q5()).list.is_empty());
    // SAFETY: null arrays with n > 0 are refused.
    assert_eq!(
        unsafe { mailsearch_set_languages(g, std::ptr::null(), std::ptr::null(), 1) },
        MAILSEARCH_E_ARG
    );
    close(g);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn people_suggestions_and_owner() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let people = |p: &str| -> Vec<u64> {
        let mut ids = [0u64; 8];
        // SAFETY: valid pointers.
        let n = unsafe {
            mailsearch_people(
                g,
                p.as_ptr() as *const c_char,
                p.len(),
                ids.as_mut_ptr(),
                ids.len(),
            )
        };
        assert!(n >= 0);
        ids[..n as usize].to_vec()
    };
    assert_eq!(people("ali"), vec![100, 102], "affinity first");
    assert_eq!(people("smi"), vec![100]);
    assert_eq!(people("bob j"), vec![101]);
    assert_eq!(people("example.net"), vec![101]);
    assert!(people("zz").is_empty());
    let addrs = ["Carol@Example.org".as_ptr() as *const c_char];
    let lens = ["Carol@Example.org".len()];
    // SAFETY: valid arrays.
    assert_eq!(
        unsafe { mailsearch_set_owner(g, addrs.as_ptr(), lens.as_ptr(), 1) },
        MAILSEARCH_OK
    );
    assert_eq!(search(g, &[c("invoice")], q5()).rc, MAILSEARCH_OK);
    close(g);
}

#[test]
fn people_one_letter() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let people = |p: &str, cap: usize| -> Vec<u64> {
        let mut ids = vec![0u64; cap];
        // SAFETY: valid pointers.
        let n = unsafe {
            mailsearch_people(
                g,
                p.as_ptr() as *const c_char,
                p.len(),
                ids.as_mut_ptr(),
                ids.len(),
            )
        };
        assert!(n >= 0);
        ids.truncate(n as usize);
        ids
    };
    // the per-letter table is built after the affinity, on its thread; a cap above 8 always runs the query
    std::thread::sleep(std::time::Duration::from_millis(200));
    for (p, want) in [
        ("a", vec![100, 102]),
        ("A", vec![100, 102]),
        ("b", vec![101, 102]),
        ("e", vec![100, 101, 102]),
        ("c", vec![100]),
        ("z", vec![]),
    ] {
        assert_eq!(people(p, 8), want, "{p}");
        assert_eq!(people(p, 9), want, "{p} through the query");
        assert_eq!(people(p, 1), want[..want.len().min(1)], "{p} cap 1");
    }
    for c in ('a'..='z').chain('0'..='9') {
        let p = c.to_string();
        assert_eq!(people(&p, 8), people(&p, 9), "{p}");
    }
    close(g);
}

#[test]
fn highlight_spans() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let hl = |cls: &[C], text: &str| -> Vec<MailsearchSpan> {
        let gc: Vec<MailsearchClause> = cls
            .iter()
            .map(|c| MailsearchClause {
                text: c.text.as_ptr() as *const c_char,
                len: c.text.len(),
                fields: c.fields,
                mode: c.mode,
                flags: c.flags,
                group: c.group,
                near: c.near,
            })
            .collect();
        let q = MailsearchQuery {
            clauses: gc.as_ptr(),
            n_clauses: gc.len(),
            now: now(),
            date_lo: 0,
            date_hi: 0,
            hint_lo: 0,
            hint_hi: 0,
            before_key: 0,
            account: 0,
            flags: 0,
            n_top: 0,
        };
        let mut sp = vec![MailsearchSpan::default(); 16];
        // SAFETY: valid pointers.
        let n = unsafe {
            mailsearch_highlight(
                g,
                &q,
                text.as_ptr() as *const c_char,
                text.len(),
                sp.as_mut_ptr(),
                sp.len(),
            )
        };
        assert!(n >= 0);
        sp.truncate(n as usize);
        sp
    };
    let t = "Invoice: Mu\u{0308}ller paid INVOICES";
    let s = hl(&[c("invoice"), c("müller")], t);
    let got: Vec<(&str, u32)> = s
        .iter()
        .map(|x| (&t[x.start as usize..x.end as usize], x.tier))
        .collect();
    assert_eq!(
        got,
        vec![
            ("Invoice", engine::T_EXACT),
            ("Mu\u{0308}ller", engine::T_EXACT),
            ("INVOICES", engine::T_PREFIX)
        ]
    );
    let t = "明日の会議室";
    let s = hl(&[c("会議")], t);
    assert_eq!(s.len(), 1);
    assert_eq!(&t[s[0].start as usize..s[0].end as usize], "会議");
    let mut p = c("alice@example.com");
    p.mode = 2;
    let t = "mail alice@example.com now";
    let s = hl(&[p], t);
    assert_eq!(
        &t[s[0].start as usize..s[0].end as usize],
        "alice@example.com"
    );
    // AROUND highlights each word where it occurs
    let mut p = c("please invoice");
    p.mode = 4;
    p.near = 2;
    let t = "Please pay the invoice";
    let got: Vec<&str> = hl(&[p], t)
        .iter()
        .map(|x| &t[x.start as usize..x.end as usize])
        .collect();
    assert_eq!(got, vec!["Please", "invoice"]);
    close(g);
}

#[test]
fn reload_lease_and_release() {
    let fx = fixture();
    let g = open(&fx.root).unwrap();
    let (mut d, mut gen0, mut gen1, mut n, mut bytes) = (0u32, 0u64, 0u64, 0u64, 0u64);
    // SAFETY: valid pointers.
    unsafe {
        assert_eq!(mailsearch_reload(g, &mut d, &mut gen0), MAILSEARCH_OK);
        assert_eq!(d, 1);
        assert_eq!(mailsearch_num_docs(g, &mut n), MAILSEARCH_OK);
        assert_eq!(n, corpus().len() as u64);
        assert_eq!(mailsearch_release(g, &mut bytes), MAILSEARCH_OK);
    }
    // the indexer commits one more document
    {
        let index = tantivy::Index::open_in_dir(fx.root.join("idx-1/mail")).unwrap();
        register(&index);
        let f = Fields::new(&index.schema()).unwrap();
        let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
        w.add_document(to_tantivy(&f, &doc(99, 399, "fresh", "fresh")))
            .unwrap();
        w.commit().unwrap();
    }
    // SAFETY: valid pointers.
    unsafe {
        assert_eq!(mailsearch_reload(g, &mut d, &mut gen1), MAILSEARCH_OK);
        assert_eq!(mailsearch_num_docs(g, &mut n), MAILSEARCH_OK);
    }
    assert!(gen1 > gen0);
    assert_eq!(n, corpus().len() as u64 + 1);
    // a rebuild switches CURRENT to idx-2
    write_index(&fx.root, 2, &corpus()[..3], false);
    // SAFETY: valid pointers.
    unsafe {
        assert_eq!(mailsearch_reload(g, &mut d, &mut gen1), MAILSEARCH_OK);
        assert_eq!(mailsearch_num_docs(g, &mut n), MAILSEARCH_OK);
    }
    assert_eq!((d, n), (2, 3));
    close(g);
}

/// Bytes of the prefetched kinds of segment files under `dir`.
fn prefetch_bytes(dir: &Path) -> u64 {
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            n += prefetch_bytes(&p);
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| engine::PREFETCH_EXTS.contains(&x))
        {
            n += e.metadata().unwrap().len();
        }
    }
    n
}

#[test]
fn prefetch_reads_the_open_segment_files() {
    let fx = fixture();
    let want = prefetch_bytes(&fx.root.join("idx-1"));
    assert!(want > 0);
    let mut e = Engine::open(&fx.root).unwrap();
    let (n, t) = e.prefetch().unwrap().unwrap();
    assert_eq!(n, want);
    t.join().unwrap();
    // the next prefetch runs once the last one ended; after a switch it reads the new directory's files
    write_index(&fx.root, 2, &corpus()[..3], false);
    e.reload().unwrap();
    let (n, t) = e.prefetch().unwrap().unwrap();
    assert_eq!(n, prefetch_bytes(&fx.root.join("idx-2")));
    t.join().unwrap();

    let g = open(&fx.root).unwrap();
    let mut b = u64::MAX;
    // SAFETY: valid pointers (null bytes is the error case).
    unsafe {
        assert_eq!(mailsearch_prefetch(g, &mut b), MAILSEARCH_OK);
        assert_eq!(
            mailsearch_prefetch(g, std::ptr::null_mut()),
            MAILSEARCH_E_ARG
        );
        assert_eq!(
            mailsearch_prefetch(std::ptr::null_mut(), &mut b),
            MAILSEARCH_E_ARG
        );
    }
    assert_eq!(b, prefetch_bytes(&fx.root.join("idx-2")));
    close(g);
}

#[test]
fn cancel_from_another_thread() {
    let root = std::env::temp_dir().join(format!("mailsearch-abi-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let docs: Vec<DocInput> = (1..=30_000u64)
        .map(|i| doc(i, (i % 390) as i64, "common words", "common body"))
        .collect();
    write_index(&root, 1, &docs, false);
    let fx = Fixture { root };
    let g = open(&fx.root).unwrap();
    let gp = g as usize;
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let t = std::thread::spawn(move || {
        while !s2.load(Ordering::Relaxed) {
            // SAFETY: the handle outlives this thread (joined before mailsearch_close); mailsearch_cancel only
            // stores into the atomic flag.
            unsafe { mailsearch_cancel(gp as *mut MailsearchHandle) };
            std::hint::spin_loop();
        }
    });
    let mut cancelled = false;
    for _ in 0..200 {
        let o = search(g, &[c("common")], q5());
        if o.rc == MAILSEARCH_E_CANCELLED {
            cancelled = true;
            break;
        }
        assert_eq!(o.rc, MAILSEARCH_OK);
    }
    stop.store(true, Ordering::Relaxed);
    t.join().unwrap();
    assert!(cancelled);
    // the flag clears when the next search starts
    let o = search(g, &[c("common")], q5());
    assert_eq!(o.rc, MAILSEARCH_OK);
    assert_eq!(o.count, 10_000);
    assert_eq!(o.capped, 1);
    assert!(o.flags & MAILSEARCH_R_FROM_RECENT != 0);
    assert_eq!(o.top.len(), 5);
    close(g);
}

#[test]
fn oracle_record() {
    let fx = fixture();
    let mut e = Engine::open(&fx.root).unwrap();
    let q = Query {
        clauses: vec![engine::Clause {
            text: "invoice".into(),
            fields: engine::F_TEXT,
            ..Default::default()
        }],
        now: now(),
        ..Default::default()
    };
    let rec = e
        .oracle(&q, &Opts::default(), &[1u64].into_iter().collect())
        .unwrap();
    assert!(rec.retrievable);
    assert_eq!(rec.n_match, 3);
    let j = rec.to_json("q1", "word");
    assert!(j.starts_with("{\"qid\":\"q1\",\"class\":\"word\""), "{j}");
    assert!(j.contains("\"bm_better\":"), "{j}");
}

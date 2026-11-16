/* libmailsearch: the C ABI of the mail search reader.
 *
 * The caller's search worker thread owns a handle and makes every call on it (mailsearch_cancel and the
 * stateless helpers excepted). The library opens `$data/search/` read-only (CURRENT -> idx-N), refuses
 * an index stamped by another norm / layout / Tantivy version (MAILSEARCH_E_VERSION: the caller rebuilds the index), and returns message ids only:
 * the caller resolves every result in its own store. Queries are typed clause lists built by the caller's query compiler; no
 * query string is parsed here. No panic crosses this boundary: a panic returns MAILSEARCH_E_PANIC and poisons
 * the handle (MAILSEARCH_E_POISONED until mailsearch_close). Text arguments are UTF-8 with explicit lengths (no NUL
 * needed); invalid UTF-8 is MAILSEARCH_E_ARG.
 */
#ifndef MAILSEARCH_H
#define MAILSEARCH_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
  MAILSEARCH_OK = 0,
  MAILSEARCH_E_ARG = -1,       /* null pointer, bad enum/flag, invalid UTF-8, capacity too small */
  MAILSEARCH_E_IO = -2,        /* index missing or unreadable (no CURRENT, unreadable segment) */
  MAILSEARCH_E_VERSION = -3,   /* VERSION missing, malformed or different: fail closed */
  MAILSEARCH_E_PANIC = -4,     /* the call panicked; the handle is now poisoned */
  MAILSEARCH_E_POISONED = -5,  /* an earlier call on this handle panicked */
  MAILSEARCH_E_CANCELLED = -6, /* mailsearch_cancel was called while mailsearch_search ran */
  MAILSEARCH_E_CORRUPT = -7,   /* a segment file failed its checksum (open, reload): the caller rebuilds the index */
};

typedef struct MailsearchHandle MailsearchHandle;

/* Clause fields (bitmask). MAILSEARCH_F_TEXT = bare terms / text: (subject, people, body, quoted,
 * attachment names and text, links, numbers). */
enum {
  MAILSEARCH_F_SUBJECT = 1u << 0,
  MAILSEARCH_F_FROM = 1u << 1,
  MAILSEARCH_F_TO = 1u << 2,
  MAILSEARCH_F_CC = 1u << 3,
  MAILSEARCH_F_BCC = 1u << 4,
  MAILSEARCH_F_BODY = 1u << 5,     /* own + forwarded text */
  MAILSEARCH_F_QUOTED = 1u << 6,   /* quoted text and signatures */
  MAILSEARCH_F_ATT_NAME = 1u << 7,
  MAILSEARCH_F_ATT_TEXT = 1u << 8,
  MAILSEARCH_F_LINKS = 1u << 9,
  MAILSEARCH_F_NUMBERS = 1u << 10,
  MAILSEARCH_F_TEXT = (1u << 11) - 1,
};

/* Clause match mode. */
enum {
  MAILSEARCH_M_DEFAULT = 0, /* exact + prefix (>= 2 chars) + stem and typo tiers (pause/Enter only) */
  MAILSEARCH_M_EXACT = 1,   /* `+word`: exact token only */
  MAILSEARCH_M_PHRASE = 2,  /* "a b": exact tokens in order, no prefix/stem/typo */
  MAILSEARCH_M_PREFIX = 3,  /* `word*`: explicit prefix */
  MAILSEARCH_M_NEAR = 4,    /* `a AROUND n b`: the word tokens of `text` at most `near` tokens apart, either order */
};

/* Clause flags. */
enum {
  MAILSEARCH_C_PARTIAL = 1u << 0, /* the word is still being typed (prefix only, hpfx path for <= 3 letters) */
  MAILSEARCH_C_NEGATE = 1u << 1,  /* NOT */
};

typedef struct MailsearchClause {
  const char* text; /* raw user text of one term or phrase; the library normalises and tokenises */
  size_t len;
  uint32_t fields;  /* MAILSEARCH_F_* */
  uint32_t mode;    /* MAILSEARCH_M_* */
  uint32_t flags;   /* MAILSEARCH_C_* */
  uint32_t group;   /* 0: its own AND-ed clause; equal non-zero groups are OR-ed, the group AND-ed */
  uint32_t near;    /* MAILSEARCH_M_NEAR: the most tokens between the words; ignored by the other modes */
} MailsearchClause;

/* Query flags. */
enum {
  MAILSEARCH_Q_TYPING = 1u << 0,      /* typing path: hpfx, no stem/typo, early termination */
  MAILSEARCH_Q_SKIP_TOP = 1u << 1,    /* typing pass 1: date list and count only */
  MAILSEARCH_Q_SKIP_LIST = 1u << 2,   /* typing pass 2: Top hits only */
  MAILSEARCH_Q_RELEVANCE = 1u << 3,   /* list in relevance order (no recency, quoted weight 0.25) */
  MAILSEARCH_Q_NO_COLLAPSE = 1u << 4, /* no result-time duplicate collapse */
  MAILSEARCH_Q_NO_EXCLUDE = 1u << 5,  /* ignore the excluded-key set (in:anywhere) */
};

typedef struct MailsearchQuery {
  const MailsearchClause* clauses;
  size_t n_clauses;
  int64_t now;                /* unix seconds; recency and relative ranking */
  int64_t date_lo, date_hi;   /* DateRange filter on received time, [lo, hi) unix seconds; 0, 0 = none */
  int64_t hint_lo, hint_hi;   /* DateHint (removed date chip, feature D); 0, 0 = none */
  int64_t before_key;         /* date-order paging: only search_key < before_key; 0 = from the newest */
  uint64_t account;           /* 0 = every account */
  uint32_t flags;             /* MAILSEARCH_Q_* */
  uint32_t n_top;             /* Top hits wanted, 0..7 */
} MailsearchQuery;

/* Tier bits in MailsearchHit.tiers. */
enum { MAILSEARCH_T_EXACT = 1u << 0, MAILSEARCH_T_PREFIX = 1u << 1, MAILSEARCH_T_STEM = 1u << 2, MAILSEARCH_T_TYPO = 1u << 3 };

typedef struct MailsearchHit {
  uint64_t mid;        /* message id */
  int64_t search_key;  /* messages.search_key */
  float score;         /* final score (0 in date order) */
  uint32_t tiers;      /* MAILSEARCH_T_* that matched */
  uint32_t fields;     /* MAILSEARCH_F_* that matched */
  uint32_t copies;     /* collapsed copies folded into this row (0 = none) */
} MailsearchHit;

/* Result flags. */
enum {
  MAILSEARCH_R_FROM_RECENT = 1u << 0, /* candidate cap applied: Top hits ranked from recent matches */
  MAILSEARCH_R_CORRECTED = 1u << 1,   /* the typo tier contributed results */
};

typedef struct MailsearchResult {
  MailsearchHit* top;        /* caller-owned, top_cap entries */
  size_t top_cap;
  size_t n_top;      /* out */
  MailsearchHit* list;       /* caller-owned: All results, date order (default) or relevance order */
  size_t list_cap;
  size_t n_list;     /* out */
  uint64_t count;    /* out: matches (after exclusion and collapse), exact up to 10,000 */
  uint32_t count_capped; /* out: 1 when count stopped at 10,000 */
  uint32_t flags;    /* out: MAILSEARCH_R_* */
} MailsearchResult;

typedef struct MailsearchSpan {
  uint32_t start, end; /* byte offsets into the text */
  uint32_t tier;       /* MAILSEARCH_T_* of the match */
} MailsearchSpan;

/* norm_version and layout_version this library reads. */
int mailsearch_versions(uint32_t* norm_version, uint32_t* layout_version);

/* The active directory number N of root ($data/search/CURRENT names idx-N), without opening it: a caller
 * that guards against a crash inside the reader keys its guard on it. MAILSEARCH_E_IO when CURRENT is
 * missing or malformed. */
int mailsearch_current(const char* root, size_t root_len, uint32_t* dir);
/* root = $data/search. NULL with *err set on failure (MAILSEARCH_E_VERSION fails closed; MAILSEARCH_E_CORRUPT:
 * a segment file failed its checksum). A checksum stops corruption, not a crafted index: Tantivy can abort
 * the process on one (README "Known limits"). */
MailsearchHandle* mailsearch_open(const char* root, size_t root_len, int* err);
/* Own addresses (mailbox identities) for the own-sent feature; replaces the previous set. */
int mailsearch_set_owner(MailsearchHandle* g, const char* const* addrs, const size_t* lens, size_t n);
/* Snowball languages of the stem tier besides English, as ISO 639-1 codes ("de", "fr", ...; da de el es fi fr hu it
 * nl no pt ro ru sv tr are known, others are ignored); replaces the previous set. English is always on. */
int mailsearch_set_languages(MailsearchHandle* g, const char* const* codes, const size_t* lens, size_t n);
/* Excluded message id set (Trash, Junk, out-of-scope folders), applied inside the collector;
 * replaces the previous set. */
int mailsearch_set_excluded(MailsearchHandle* g, const uint64_t* mids, size_t n);
int mailsearch_search(MailsearchHandle* g, const MailsearchQuery* q, MailsearchResult* r);
/* Thread-safe: makes the mailsearch_search running on g return MAILSEARCH_E_CANCELLED (checked between segments and
 * every few thousand documents). The flag clears when the next mailsearch_search starts. */
int mailsearch_cancel(MailsearchHandle* g);
/* People suggestions: correspondents.id of names/addresses matching prefix, best first. Returns the
 * number written (>= 0) or an error. */
int mailsearch_people(MailsearchHandle* g, const char* prefix, size_t len, uint64_t* ids, size_t cap);
/* Byte spans of `text` that the query's clauses match (same tokenizer as indexing), for snippets.
 * Returns the number written (>= 0) or an error. */
int mailsearch_highlight(MailsearchHandle* g, const MailsearchQuery* q, const char* text, size_t len, MailsearchSpan* spans, size_t cap);
/* After IndexCommitted: reopen at the newest commit; also releases the pages of the old mapping.
 * (*dir, *gen) is the lease to report as ReaderAt. Errors as mailsearch_open. */
int mailsearch_reload(MailsearchHandle* g, uint32_t* dir, uint64_t* gen);
/* madvise(MADV_DONTNEED) every mapping + malloc_trim: after each reader reopen and at the end of a
 * typing session. *bytes = mapped bytes advised. */
int mailsearch_release(MailsearchHandle* g, uint64_t* bytes);
/* Starts reading the open segments' term dictionaries, fast fields and fieldnorms into the page cache on a
 * background thread and returns at once, so a first query after the page cache was dropped does not wait on the
 * disk: at open, after mailsearch_reload and at the first edit of a typing session. Nothing is mapped into the
 * process. *bytes = bytes it will read; 0 while an earlier prefetch is still running. */
int mailsearch_prefetch(MailsearchHandle* g, uint64_t* bytes);
int mailsearch_num_docs(MailsearchHandle* g, uint64_t* n);
int mailsearch_close(MailsearchHandle* g);
/* Message of the last error on this thread; returns its full length (may exceed len). */
size_t mailsearch_last_error(char* buf, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* MAILSEARCH_H */

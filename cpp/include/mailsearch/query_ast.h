// The one immutable search AST. A search box, its chips, saved searches and quick filters all build a
// Query; the caller's compiler turns it into reader clauses (mailsearch.h) plus its own structured
// filters. A Query is a value: every edit (removing a chip, the Sort toggle) produces a new one.
//
// The node tree is what canonical text covers (text → AST → canonical → AST is the identity).
// Scope, Sort and DateHint are carried beside the tree by the consumer: Scope comes from the scope
// chip, Sort from the toggle (saved searches store it separately), and a
// DateHint is what remains of a removed date chip.
#pragma once

#include <cstddef>
#include <cstdint>
#include <limits>
#include <optional>
#include <string>
#include <vector>

namespace mailsearch {

// Bumped whenever canonical text or its meaning changes (store it with saved queries).
inline constexpr uint32_t kGrammarVersion = 1;
// Bounds (the parser never fails: input past them is ignored with a kTruncated hint).
inline constexpr size_t kMaxQueryBytes = 4096;  // largest query text (also the saved-query bound)
inline constexpr size_t kMaxNodes = 256;  // leaf and Not nodes; And/Or groups come on top
inline constexpr size_t kMaxDepth = 16;
inline constexpr size_t kMaxListItems = 32;  // values in one `from:a,b,…` / addresses in a Person

// Open ends of a DateRange.
inline constexpr int64_t kOpenFrom = std::numeric_limits<int64_t>::min();
inline constexpr int64_t kOpenTo = std::numeric_limits<int64_t>::max();

enum class NodeKind : uint8_t { kAnd, kOr, kNot, kTerm, kPerson, kFilter, kDateRange };

// Text keyword of a Term; kText = bare terms.
enum class Field : uint8_t {
  kText,
  kContent,
  kSubject,
  kBody,
  kFrom,
  kTo,
  kCc,
  kBcc,
  kRecipient,
  kEmail,
  kAttachmentName,
  kAttachmentContent,
};

enum class TermMode : uint8_t { kDefault, kExact, kPrefix, kPhrase, kNear, kWildcard };
enum class Script : uint8_t { kWord, kCjk, kMixed };

// Person roles (bitmask).
inline constexpr uint32_t kRoleFrom = 1u << 0;
inline constexpr uint32_t kRoleTo = 1u << 1;
inline constexpr uint32_t kRoleCc = 1u << 2;
inline constexpr uint32_t kRoleBcc = 1u << 3;
inline constexpr uint32_t kRoleAny = kRoleFrom | kRoleTo | kRoleCc | kRoleBcc;

// Structured filters (applied by the caller, e.g. as SQL on its result join).
enum class FilterKey : uint8_t {
  kAttachment,   // value yes|no
  kHas,          // pdf, image, document, spreadsheet, presentation, archive, calendar, link
  kRead,         // yes|no
  kFlagged,      // yes|no
  kIs,           // sent, draft, replied, bulk
  kSigned,       // yes|no
  kEncrypted,    // yes|no
  kImportance,   // low|medium|high
  kTag,          // tag or Gmail label name
  kFolder,       // inbox, sent, drafts, archive, trash, junk, anywhere, or a folder path
  kAccount,      // account name or address
  kLarger,       // bytes
  kSmaller,      // bytes
  kMsgId,        // Message-ID without angle brackets
  kReply,        // Reply-To name or address
  kDeliveredTo,  // account identity
  kList,         // List-Id
  kUnavailable,  // recognised but not supported: value = "keyword:value" as typed
};

enum class DateField : uint8_t { kSent, kReceived };
enum class DateOp : uint8_t { kOn, kBefore, kAfter };

struct Node {
  NodeKind kind = NodeKind::kTerm;
  std::vector<uint32_t> children;  // And, Or (>= 2), Not (1)

  // Term
  std::string text;  // the user's word(s); phrase / near: words separated by one space
  Field field = Field::kText;
  TermMode mode = TermMode::kDefault;
  uint32_t near = 0;  // kNear: within n tokens
  Script script = Script::kWord;

  // Person: exact addresses (lowercased) in the given roles. `me` = the user's own identities.
  std::vector<std::string> addresses;
  uint32_t roles = 0;
  bool me = false;

  // Filter
  FilterKey key = FilterKey::kTag;
  std::string value;  // normalised as listed above; kLarger/kSmaller: decimal bytes

  // DateRange: [from, to) in unix seconds (kOpenFrom / kOpenTo = open), resolved from source_text
  // against ParseOptions.now; canonical text prints source_text, so relative dates re-resolve.
  DateField date_field = DateField::kReceived;
  DateOp date_op = DateOp::kOn;
  int64_t from = kOpenFrom, to = kOpenTo;
  std::string source_text;
};

// Scope chip: empty folders and accounts = all. Trash and Junk are excluded unless
// include_trash_junk or the query names them (`in:trash`, `in:anywhere`).
struct Scope {
  std::vector<int64_t> accounts;
  std::vector<int64_t> folders;
  bool subfolders = false;
  bool include_trash_junk = false;
  bool operator==(const Scope&) const = default;
};

enum class Sort : uint8_t { kDate, kRelevance };

// Left behind when a date chip is removed: only raises Top hits inside [from, to).
struct DateHint {
  int64_t from = 0, to = 0;
  bool operator==(const DateHint&) const = default;
};

struct Query {
  std::vector<Node> nodes;  // nodes[root] is the tree; an empty query has no nodes
  std::optional<uint32_t> root;
  Scope scope;
  Sort sort = Sort::kDate;
  std::optional<DateHint> hint;

  bool empty() const { return !root.has_value(); }
  const Node& node(uint32_t i) const { return nodes[i]; }
};

// Structural equality of the trees (node order in `nodes` may differ) plus scope, sort and hint.
bool same_query(const Query& a, const Query& b);

// Byte range in the parsed text.
struct Span {
  uint32_t begin = 0, end = 0;
  bool operator==(const Span&) const = default;
};

// Inline hints the box shows on tokens.
enum class HintKind : uint8_t {
  kTreatedAsText,  // unknown or malformed syntax degraded to free text
  kNotAvailable,   // keyword recognised but not supported ("not available")
  kTruncated,      // input past kMaxQueryBytes / kMaxNodes / kMaxDepth ignored
  kIgnored,        // a stray `)` or `}`, an empty group
};

struct Hint {
  HintKind kind = HintKind::kTreatedAsText;
  Span span;
};

struct ParseResult {
  Query query;
  std::vector<Span> spans;  // per node: the text it came from (chips, the partial last word)
  std::vector<Hint> hints;
  bool keyword_list = false;  // the text starts with `?`: open the keyword list
};

}  // namespace mailsearch

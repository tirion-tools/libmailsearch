#include "mailsearch/query_parser.h"

#include <algorithm>
#include <array>
#include <charconv>
#include <optional>

namespace mailsearch {
namespace {

// ------------------------------------------------------------------------------ keyword table

enum class KwKind : uint8_t { kText, kFilter, kHas, kIs, kDate, kUnavailable };

struct Keyword {
  std::string_view name;  // lowercase
  KwKind kind = KwKind::kText;
  Field field = Field::kText;
  FilterKey key = FilterKey::kTag;
  DateField date_field = DateField::kReceived;
  DateOp date_op = DateOp::kOn;
};

constexpr std::array kKeywords = {
    Keyword{.name = "text", .kind = KwKind::kText, .field = Field::kText},
    Keyword{.name = "content", .kind = KwKind::kText, .field = Field::kContent},
    Keyword{.name = "subject", .kind = KwKind::kText, .field = Field::kSubject},
    Keyword{.name = "body", .kind = KwKind::kText, .field = Field::kBody},
    Keyword{.name = "from", .kind = KwKind::kText, .field = Field::kFrom},
    Keyword{.name = "to", .kind = KwKind::kText, .field = Field::kTo},
    Keyword{.name = "cc", .kind = KwKind::kText, .field = Field::kCc},
    Keyword{.name = "bcc", .kind = KwKind::kText, .field = Field::kBcc},
    Keyword{.name = "recipient", .kind = KwKind::kText, .field = Field::kRecipient},
    Keyword{.name = "email", .kind = KwKind::kText, .field = Field::kEmail},
    Keyword{.name = "with", .kind = KwKind::kText, .field = Field::kEmail},
    Keyword{.name = "participant", .kind = KwKind::kText, .field = Field::kEmail},
    Keyword{.name = "attachmentname", .kind = KwKind::kText, .field = Field::kAttachmentName},
    Keyword{.name = "filename", .kind = KwKind::kText, .field = Field::kAttachmentName},
    Keyword{.name = "attachmentcontent", .kind = KwKind::kText, .field = Field::kAttachmentContent},
    Keyword{.name = "attached", .kind = KwKind::kText, .field = Field::kAttachmentContent},
    Keyword{.name = "reply", .kind = KwKind::kFilter, .key = FilterKey::kReply},
    Keyword{.name = "deliveredto", .kind = KwKind::kFilter, .key = FilterKey::kDeliveredTo},
    Keyword{.name = "list", .kind = KwKind::kFilter, .key = FilterKey::kList},
    Keyword{.name = "attachment", .kind = KwKind::kFilter, .key = FilterKey::kAttachment},
    Keyword{.name = "has", .kind = KwKind::kHas, .key = FilterKey::kHas},
    Keyword{.name = "read", .kind = KwKind::kFilter, .key = FilterKey::kRead},
    Keyword{.name = "flag", .kind = KwKind::kFilter, .key = FilterKey::kFlagged},
    Keyword{.name = "is", .kind = KwKind::kIs, .key = FilterKey::kIs},
    Keyword{.name = "signed", .kind = KwKind::kFilter, .key = FilterKey::kSigned},
    Keyword{.name = "encrypted", .kind = KwKind::kFilter, .key = FilterKey::kEncrypted},
    Keyword{.name = "importance", .kind = KwKind::kFilter, .key = FilterKey::kImportance},
    Keyword{.name = "tag", .kind = KwKind::kFilter, .key = FilterKey::kTag},
    Keyword{.name = "label", .kind = KwKind::kFilter, .key = FilterKey::kTag},
    Keyword{.name = "folder", .kind = KwKind::kFilter, .key = FilterKey::kFolder},
    Keyword{.name = "in", .kind = KwKind::kFilter, .key = FilterKey::kFolder},
    Keyword{.name = "account", .kind = KwKind::kFilter, .key = FilterKey::kAccount},
    Keyword{.name = "before", .kind = KwKind::kDate, .date_field = DateField::kSent, .date_op = DateOp::kBefore},
    Keyword{.name = "older", .kind = KwKind::kDate, .date_field = DateField::kSent, .date_op = DateOp::kBefore},
    Keyword{.name = "after", .kind = KwKind::kDate, .date_field = DateField::kSent, .date_op = DateOp::kAfter},
    Keyword{.name = "newer", .kind = KwKind::kDate, .date_field = DateField::kSent, .date_op = DateOp::kAfter},
    Keyword{.name = "received-before", .kind = KwKind::kDate, .date_field = DateField::kReceived, .date_op = DateOp::kBefore},
    Keyword{.name = "received-after", .kind = KwKind::kDate, .date_field = DateField::kReceived, .date_op = DateOp::kAfter},
    Keyword{.name = "received-within", .kind = KwKind::kDate, .date_field = DateField::kReceived, .date_op = DateOp::kAfter},
    Keyword{.name = "within", .kind = KwKind::kDate, .date_field = DateField::kReceived, .date_op = DateOp::kAfter},
    Keyword{.name = "newer_than", .kind = KwKind::kDate, .date_field = DateField::kReceived, .date_op = DateOp::kAfter},
    Keyword{.name = "older_than", .kind = KwKind::kDate, .date_field = DateField::kReceived, .date_op = DateOp::kBefore},
    Keyword{.name = "date", .kind = KwKind::kDate, .date_field = DateField::kReceived, .date_op = DateOp::kOn},
    Keyword{.name = "larger", .kind = KwKind::kFilter, .key = FilterKey::kLarger},
    Keyword{.name = "size", .kind = KwKind::kFilter, .key = FilterKey::kLarger},
    Keyword{.name = "smaller", .kind = KwKind::kFilter, .key = FilterKey::kSmaller},
    Keyword{.name = "msgid", .kind = KwKind::kFilter, .key = FilterKey::kMsgId},
    Keyword{.name = "rfc822msgid", .kind = KwKind::kFilter, .key = FilterKey::kMsgId},
    // Recognised, not supported: a "not available" chip, never dropped.
    Keyword{.name = "notes", .kind = KwKind::kUnavailable},
    Keyword{.name = "category", .kind = KwKind::kUnavailable},
    Keyword{.name = "header", .kind = KwKind::kUnavailable},
};

// Display spellings of the keywords whose canonical name is not all lowercase.
std::string_view display_name(std::string_view lower) {
  if (lower == "attachmentname") return "attachmentName";
  if (lower == "attachmentcontent") return "attachmentContent";
  return lower;
}

const Keyword* find_keyword(std::string_view lower) {
  for (const Keyword& k : kKeywords) {
    if (k.name == lower) return &k;
  }
  return nullptr;
}

std::string_view field_name(Field f) {
  switch (f) {
    case Field::kText: return "text";
    case Field::kContent: return "content";
    case Field::kSubject: return "subject";
    case Field::kBody: return "body";
    case Field::kFrom: return "from";
    case Field::kTo: return "to";
    case Field::kCc: return "cc";
    case Field::kBcc: return "bcc";
    case Field::kRecipient: return "recipient";
    case Field::kEmail: return "email";
    case Field::kAttachmentName: return "attachmentName";
    case Field::kAttachmentContent: return "attachmentContent";
  }
  return "text";
}

std::string_view filter_name(FilterKey k) {
  switch (k) {
    case FilterKey::kAttachment: return "attachment";
    case FilterKey::kHas: return "has";
    case FilterKey::kRead: return "read";
    case FilterKey::kFlagged: return "flag";
    case FilterKey::kIs: return "is";
    case FilterKey::kSigned: return "signed";
    case FilterKey::kEncrypted: return "encrypted";
    case FilterKey::kImportance: return "importance";
    case FilterKey::kTag: return "tag";
    case FilterKey::kFolder: return "folder";
    case FilterKey::kAccount: return "account";
    case FilterKey::kLarger: return "larger";
    case FilterKey::kSmaller: return "smaller";
    case FilterKey::kMsgId: return "msgid";
    case FilterKey::kReply: return "reply";
    case FilterKey::kDeliveredTo: return "deliveredto";
    case FilterKey::kList: return "list";
    case FilterKey::kUnavailable: return "";
  }
  return "";
}

std::string_view date_name(DateField f, DateOp op) {
  if (f == DateField::kSent) return op == DateOp::kBefore ? "before" : "after";  // sent + on: not expressible
  switch (op) {
    case DateOp::kBefore: return "received-before";
    case DateOp::kAfter: return "received-after";
    case DateOp::kOn: return "date";
  }
  return "date";
}

uint32_t person_roles(Field f) {
  switch (f) {
    case Field::kFrom: return kRoleFrom;
    case Field::kTo: return kRoleTo;
    case Field::kCc: return kRoleCc;
    case Field::kBcc: return kRoleBcc;
    case Field::kRecipient: return kRoleTo | kRoleCc | kRoleBcc;
    case Field::kEmail: return kRoleAny;
    default: return 0;
  }
}

std::string_view person_keyword(uint32_t roles) {
  switch (roles) {
    case kRoleFrom: return "from";
    case kRoleTo: return "to";
    case kRoleCc: return "cc";
    case kRoleBcc: return "bcc";
    case kRoleTo | kRoleCc | kRoleBcc: return "recipient";
    default: return "email";
  }
}

bool header_field(Field f) {
  return f == Field::kSubject || f == Field::kFrom || f == Field::kTo || f == Field::kCc || f == Field::kBcc ||
         f == Field::kRecipient || f == Field::kEmail;
}

// ------------------------------------------------------------------------------ text helpers

std::string lower_ascii(std::string_view s) {
  std::string out(s);
  for (char& c : out) {
    if (c >= 'A' && c <= 'Z') c = static_cast<char>(c - 'A' + 'a');
  }
  return out;
}

bool is_ws(char c) { return c == ' ' || c == '\t'; }
bool is_alpha(char c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
bool is_structural(char c) { return c == '(' || c == ')' || c == '{' || c == '}' || c == '"'; }

// One UTF-8 scalar at s[i]: length, or 0 when invalid.
size_t utf8_len(std::string_view s, size_t i, uint32_t* cp) {
  const auto b0 = static_cast<unsigned char>(s[i]);
  if (b0 < 0x80) {
    *cp = b0;
    return 1;
  }
  size_t n = 0;
  uint32_t v = 0;
  uint32_t min = 0;
  if ((b0 & 0xE0) == 0xC0) {
    n = 2, v = b0 & 0x1F, min = 0x80;
  } else if ((b0 & 0xF0) == 0xE0) {
    n = 3, v = b0 & 0x0F, min = 0x800;
  } else if ((b0 & 0xF8) == 0xF0) {
    n = 4, v = b0 & 0x07, min = 0x10000;
  } else {
    return 0;
  }
  if (i + n > s.size()) return 0;
  for (size_t k = 1; k < n; ++k) {
    const auto b = static_cast<unsigned char>(s[i + k]);
    if ((b & 0xC0) != 0x80) return 0;
    v = (v << 6) | (b & 0x3F);
  }
  if (v < min || v > 0x10FFFF || (v >= 0xD800 && v <= 0xDFFF)) return 0;
  *cp = v;
  return n;
}

std::string sanitize(std::string_view in, bool* truncated) {
  std::string out;
  out.reserve(std::min(in.size(), kMaxQueryBytes));
  size_t i = 0;
  while (i < in.size()) {
    uint32_t cp = 0;
    const size_t n = utf8_len(in, i, &cp);
    std::string_view piece = n == 0 ? std::string_view("\xEF\xBF\xBD") : in.substr(i, n);
    if (n != 0 && (cp < 0x20 || cp == 0x7F)) piece = " ";
    if (out.size() + piece.size() > kMaxQueryBytes) {
      *truncated = true;
      break;
    }
    out.append(piece);
    i += n == 0 ? 1 : n;
  }
  return out;
}

// Same code-point ranges as mailsearch-core's CJK routing (core/src/text.rs `is_cjk`).
bool is_cjk(uint32_t cp) {
  return (cp >= 0x3040 && cp <= 0x30FF) || (cp >= 0x31F0 && cp <= 0x31FF) || (cp >= 0x3400 && cp <= 0x4DBF) ||
         (cp >= 0x4E00 && cp <= 0x9FFF) || (cp >= 0xAC00 && cp <= 0xD7AF) || (cp >= 0xF900 && cp <= 0xFAFF) ||
         (cp >= 0xFF66 && cp <= 0xFF9F) || (cp >= 0x20000 && cp <= 0x3134F);
}

Script script_of(std::string_view s) {
  bool cjk = false;
  bool other = false;
  size_t i = 0;
  while (i < s.size()) {
    uint32_t cp = 0;
    const size_t n = utf8_len(s, i, &cp);
    if (n == 0) {
      ++i;
      continue;
    }
    if (is_cjk(cp)) {
      cjk = true;
    } else if ((cp >= '0' && cp <= '9') || (cp >= 'a' && cp <= 'z') || (cp >= 'A' && cp <= 'Z') || cp >= 0x80) {
      other = true;
    }
    i += n;
  }
  if (cjk) return other ? Script::kMixed : Script::kCjk;
  return Script::kWord;
}

std::string collapse_ws(std::string_view s) {
  std::string out;
  bool space = false;
  for (char c : s) {
    if (is_ws(c)) {
      space = !out.empty();
      continue;
    }
    if (space) out.push_back(' ');
    space = false;
    out.push_back(c);
  }
  return out;
}

bool date_start_word(std::string_view word, const ParseOptions& o) {
  const std::string w = lower_ascii(word);
  if (w.empty()) return false;
  if (w[0] >= '0' && w[0] <= '9') return true;
  for (std::string_view v : {"today", "yesterday", "this", "last", "since", "between", "q1", "q2", "q3", "q4"}) {
    if (w == v) return true;
  }
  Period p;
  const std::array<std::string_view, 1> one = {w};
  const std::array<std::string_view, 2> with_year = {w, "2024"};
  const std::array<std::string_view, 2> with_day = {w, "1"};
  return match_date_phrase(one, o.dates, &p) > 0 || match_date_phrase(with_year, o.dates, &p) > 0 ||
         match_date_phrase(with_day, o.dates, &p) > 0;
}

enum class Ctx : uint8_t { kBare, kValue };

// Escapes `text` so the lexer reads it back as one literal word of the same text.
std::string escape_word(std::string_view text, Ctx ctx, bool keep_stars, bool whole_word_rules, const ParseOptions& o) {
  std::string out;
  out.reserve(text.size() + 4);
  size_t colon = std::string_view::npos;
  if (ctx == Ctx::kBare) {
    const size_t c = text.find(':');
    if (c != std::string_view::npos && c > 0 && find_keyword(lower_ascii(text.substr(0, c)))) colon = c;
  }
  bool escape_first = false;
  if (whole_word_rules) {
    escape_first = text == "OR" || text == "AND" || text == "AROUND";
    if (ctx == Ctx::kBare && date_start_word(text, o)) escape_first = true;
    if (ctx == Ctx::kValue && lower_ascii(text) == "me") escape_first = true;
  }
  for (size_t i = 0; i < text.size(); ++i) {
    const char ch = text[i];
    bool special = ch == '\\' || is_structural(ch) || is_ws(ch) || (ch == '*' && !keep_stars) ||
                   (ctx == Ctx::kValue && ch == ',') || i == colon;
    if (i == 0) special = special || ch == '-' || ch == '+' || ch == '?' || (ctx == Ctx::kValue && ch == '<') || escape_first;
    if (special) out.push_back('\\');
    out.push_back(ch);
  }
  return out;
}

std::string quote(std::string_view text) {
  std::string out = "\"";
  for (char ch : text) {
    if (ch == '"' || ch == '\\') out.push_back('\\');
    out.push_back(ch);
  }
  out.push_back('"');
  return out;
}

// Filter / date values: quoted when they would not lex back as one value word.
std::string value_text(std::string_view v) {
  if (v.empty()) return "\"\"";
  bool plain = true;
  for (size_t i = 0; i < v.size(); ++i) {
    const char ch = v[i];
    if (is_ws(ch) || is_structural(ch) || ch == ',' || ch == '\\' || (i == 0 && (ch == '-' || ch == '<'))) plain = false;
  }
  if (plain) return std::string(v);
  // Quotes collapse whitespace when read back; a value whose spacing they would change is escaped.
  if (collapse_ws(v) == v) return quote(v);
  std::string out;
  for (size_t i = 0; i < v.size(); ++i) {
    const char ch = v[i];
    if (is_ws(ch) || is_structural(ch) || ch == ',' || ch == '\\' || (i == 0 && (ch == '-' || ch == '<'))) out.push_back('\\');
    out.push_back(ch);
  }
  return out;
}

// ------------------------------------------------------------------------------ value validation

std::optional<std::string> yes_no(std::string_view v) {
  const std::string l = lower_ascii(v);
  if (l == "yes" || l == "true" || l == "1") return "yes";
  if (l == "no" || l == "false" || l == "0") return "no";
  return std::nullopt;
}

// `20kB`, `30MB`, `1.5MB`, `512` (bytes); 1024-based units.
std::optional<std::string> size_bytes(std::string_view v) {
  size_t i = 0;
  uint64_t whole = 0;
  while (i < v.size() && v[i] >= '0' && v[i] <= '9') {
    if (whole > 1'000'000'000'000ULL) return std::nullopt;
    whole = whole * 10 + static_cast<uint64_t>(v[i] - '0');
    ++i;
  }
  if (i == 0) return std::nullopt;
  uint64_t frac = 0;
  uint64_t frac_div = 1;
  if (i < v.size() && v[i] == '.') {
    ++i;
    while (i < v.size() && v[i] >= '0' && v[i] <= '9') {
      if (frac_div < 1000) {
        frac = frac * 10 + static_cast<uint64_t>(v[i] - '0');
        frac_div *= 10;
      }
      ++i;
    }
  }
  const std::string unit = lower_ascii(v.substr(i));
  uint64_t mult = 0;
  if (unit.empty() || unit == "b") mult = 1;
  if (unit == "k" || unit == "kb" || unit == "kib") mult = 1024;
  if (unit == "m" || unit == "mb" || unit == "mib") mult = 1024 * 1024;
  if (unit == "g" || unit == "gb" || unit == "gib") mult = 1024ULL * 1024 * 1024;
  if (mult == 0) return std::nullopt;
  const uint64_t bytes = whole * mult + frac * mult / frac_div;
  if (bytes > (1ULL << 53)) return std::nullopt;
  return std::to_string(bytes);
}

struct Item {
  std::string text;  // unescaped
  std::string esc;   // per byte of text: 1 = escaped (literal)
  bool quoted = false;
  bool literal_at(size_t i) const { return esc[i] == '\1'; }
};

// ------------------------------------------------------------------------------ parser

class Parser {
 public:
  Parser(std::string_view s, const ParseOptions& o, ParseResult& out) : s_(s), o_(o), out_(out) {}

  void run() {
    skip_ws();
    if (pos_ < s_.size() && s_[pos_] == '?') {
      out_.keyword_list = true;
      ++pos_;
    }
    const auto items = seq(0, 0, false);
    out_.query.root = group(items, NodeKind::kAnd, {0, static_cast<uint32_t>(s_.size())});
  }

 private:
  Span span_from(size_t begin) const { return {static_cast<uint32_t>(begin), static_cast<uint32_t>(pos_)}; }

  void hint(HintKind k, size_t begin, size_t end) {
    out_.hints.push_back({k, {static_cast<uint32_t>(begin), static_cast<uint32_t>(end)}});
  }

  std::optional<uint32_t> add(Node n, Span sp) {
    const bool group_node = n.kind == NodeKind::kAnd || n.kind == NodeKind::kOr;
    if (!group_node && leaves_ >= kMaxNodes) {
      if (!full_) {
        full_ = true;
        hint(HintKind::kTruncated, sp.begin, s_.size());
      }
      return std::nullopt;
    }
    if (!group_node) ++leaves_;
    out_.query.nodes.push_back(std::move(n));
    out_.spans.push_back(sp);
    return static_cast<uint32_t>(out_.query.nodes.size() - 1);
  }

  std::optional<uint32_t> group(const std::vector<uint32_t>& items, NodeKind kind, Span sp) {
    if (items.empty()) return std::nullopt;
    if (items.size() == 1) return items[0];
    Node n;
    n.kind = kind;
    n.children = items;
    return add(std::move(n), sp);
  }

  std::optional<uint32_t> term(std::string text, Field f, TermMode mode, Span sp) {
    Node n;
    n.kind = NodeKind::kTerm;
    n.script = script_of(text);
    n.text = std::move(text);
    n.field = f;
    n.mode = mode;
    return add(std::move(n), sp);
  }

  void skip_ws() {
    while (pos_ < s_.size() && is_ws(s_[pos_])) ++pos_;
  }
  bool at_end() const { return pos_ >= s_.size(); }
  bool ends_word(size_t p) const { return p >= s_.size() || is_ws(s_[p]) || is_structural(s_[p]); }

  bool op_word_at(size_t p, std::string_view op) const {
    return s_.substr(p, op.size()) == op && ends_word(p + op.size());
  }
  bool op_word(std::string_view op) const { return op_word_at(pos_, op); }

  // A word up to whitespace or a structural character; `\x` is a literal x.
  Item read_word(bool stop_at_comma) {
    Item it;
    while (pos_ < s_.size()) {
      char c = s_[pos_];
      if (is_ws(c) || is_structural(c) || (stop_at_comma && c == ',')) break;
      if (c == '\\' && pos_ + 1 < s_.size()) {
        ++pos_;
        c = s_[pos_];
        it.text.push_back(c);
        it.esc.push_back('\1');
      } else {
        it.text.push_back(c);
        it.esc.push_back('\0');
      }
      ++pos_;
    }
    return it;
  }

  // "…" with `\"` and `\\`; an unterminated quote runs to the end of the input.
  Item read_quoted() {
    Item it;
    it.quoted = true;
    ++pos_;  // opening quote
    while (pos_ < s_.size() && s_[pos_] != '"') {
      if (s_[pos_] == '\\' && pos_ + 1 < s_.size()) ++pos_;
      it.text.push_back(s_[pos_]);
      ++pos_;
    }
    if (pos_ < s_.size()) ++pos_;  // closing quote
    it.text = collapse_ws(it.text);
    it.esc.assign(it.text.size(), '\1');
    return it;
  }

  std::vector<uint32_t> seq(char closer, size_t depth, bool braces) {
    std::vector<uint32_t> items;
    while (!full_) {
      skip_ws();
      if (at_end()) break;  // an unclosed group closes at the end
      const char c = s_[pos_];
      if (closer != 0 && c == closer) {
        ++pos_;
        break;
      }
      if (c == ')' || c == '}') {
        hint(HintKind::kIgnored, pos_, pos_ + 1);
        ++pos_;
        continue;
      }
      if (op_word("AND") || (braces && op_word("OR"))) {
        pos_ += op_word("AND") ? 3 : 2;
        continue;
      }
      if (op_word("OR")) {  // no left operand
        const size_t b = pos_;
        pos_ += 2;
        hint(HintKind::kTreatedAsText, b, pos_);
        if (auto t = term("OR", Field::kText, TermMode::kDefault, span_from(b))) items.push_back(*t);
        continue;
      }
      if (auto item = or_expr(depth, braces)) items.push_back(*item);
    }
    if (full_) pos_ = s_.size();
    return items;
  }

  std::optional<uint32_t> or_expr(size_t depth, bool braces) {
    const size_t begin = pos_;
    std::vector<uint32_t> alts;
    if (auto first = unary(depth, braces, false)) {
      alts.push_back(*first);
      near(*first);
    }
    while (!full_) {
      const size_t save = pos_;
      skip_ws();
      if (!op_word("OR")) {
        pos_ = save;
        break;
      }
      pos_ += 2;
      skip_ws();
      if (at_end() || s_[pos_] == ')' || s_[pos_] == '}' || op_word("OR")) {  // dangling: seq reads it as text
        pos_ = save;
        break;
      }
      if (auto next = unary(depth, braces, false)) {
        alts.push_back(*next);
        near(*next);
      }
    }
    return group(alts, NodeKind::kOr, span_from(begin));
  }

  // `a AROUND n b` after a bare word: the Term becomes near(n).
  void near(uint32_t id) {
    Node& n = out_.query.nodes[id];
    if (n.kind != NodeKind::kTerm || n.field != Field::kText || n.mode != TermMode::kDefault || n.text.find(' ') != std::string::npos) {
      return;
    }
    const size_t save = pos_;
    skip_ws();
    if (!op_word("AROUND")) {
      pos_ = save;
      return;
    }
    pos_ += 6;
    skip_ws();
    size_t p = pos_;
    while (p < s_.size() && s_[p] >= '0' && s_[p] <= '9') ++p;
    uint32_t dist = 0;
    const auto r = std::from_chars(s_.data() + pos_, s_.data() + p, dist);
    if (p == pos_ || p - pos_ > 4 || r.ec != std::errc() || !ends_word(p) || p >= s_.size() || !is_ws(s_[p])) {
      pos_ = save;
      return;
    }
    pos_ = p;
    skip_ws();
    if (at_end() || is_structural(s_[pos_]) || op_word("OR") || op_word("AND") || op_word("AROUND") || s_[pos_] == '-' ||
        s_[pos_] == '+') {
      pos_ = save;
      return;
    }
    Item w = read_word(false);
    if (w.text.empty() || w.text.find(':') != std::string::npos || w.text.find('*') != std::string::npos) {
      pos_ = save;
      return;
    }
    n.text += " " + w.text;
    n.mode = TermMode::kNear;
    n.near = dist;
    n.script = script_of(n.text);
    out_.spans[id].end = static_cast<uint32_t>(pos_);
  }

  std::optional<uint32_t> unary(size_t depth, bool braces, bool negated) {
    skip_ws();
    if (at_end() || full_) return std::nullopt;
    const size_t begin = pos_;
    const char c = s_[pos_];
    if (c == '-' || c == '+') {
      const char next = pos_ + 1 < s_.size() ? s_[pos_ + 1] : '\0';
      if (next == '\0' || is_ws(next) || next == ')' || next == '}' || (c == '-' && depth + 1 >= kMaxDepth)) {
        ++pos_;
        hint(HintKind::kIgnored, begin, pos_);
        return std::nullopt;
      }
      ++pos_;
      if (c == '-') {
        auto inner = unary(depth + 1, braces, true);
        if (!inner) return std::nullopt;
        Node n;
        n.kind = NodeKind::kNot;
        n.children = {*inner};
        return add(std::move(n), span_from(begin));
      }
      if (s_[pos_] == '"' || s_[pos_] == '(' || s_[pos_] == '{') {  // `+"…"`, `+(`: the plus means nothing
        hint(HintKind::kIgnored, begin, pos_);
        return unary(depth, braces, negated);
      }
      Item w = read_word(false);
      if (w.text.empty()) return std::nullopt;
      return term(std::move(w.text), Field::kText, TermMode::kExact, span_from(begin));
    }
    if (c == '(' || c == '{') {
      ++pos_;
      if (depth + 1 >= kMaxDepth) {
        hint(HintKind::kTruncated, begin, pos_);
        return std::nullopt;
      }
      const auto items = seq(c == '(' ? ')' : '}', depth + 1, c == '{');
      if (items.empty()) {
        hint(HintKind::kIgnored, begin, pos_);
        return std::nullopt;
      }
      return group(items, c == '{' ? NodeKind::kOr : NodeKind::kAnd, span_from(begin));
    }
    if (c == '"') {
      Item q = read_quoted();
      if (q.text.empty()) {
        hint(HintKind::kIgnored, begin, pos_);
        return std::nullopt;
      }
      return term(std::move(q.text), Field::kText, TermMode::kPhrase, span_from(begin));
    }
    if (op_word("OR") || op_word("AND") || op_word("AROUND")) {
      const size_t len = op_word("AROUND") ? 6 : (op_word("AND") ? 3 : 2);
      std::string text(s_.substr(pos_, len));
      pos_ += len;
      hint(HintKind::kTreatedAsText, begin, pos_);
      return term(std::move(text), Field::kText, TermMode::kDefault, span_from(begin));
    }
    if (auto kw = keyword(begin)) return *kw;
    if (!negated && depth == 0 && !braces && o_.bare_dates) {
      if (auto d = bare_date(begin)) return d;
    }
    Item w = read_word(false);
    return bare_term(std::move(w), Field::kText, begin);
  }

  // A bare word or one keyword value item → Term (or nothing when empty).
  std::optional<uint32_t> bare_term(Item w, Field f, size_t begin) {
    if (w.text.empty()) {
      hint(HintKind::kIgnored, begin, pos_);
      return std::nullopt;
    }
    TermMode mode = TermMode::kDefault;
    size_t stars = 0;
    for (size_t i = 0; i < w.text.size(); ++i) stars += (w.text[i] == '*' && !w.literal_at(i)) ? 1 : 0;
    if (stars > 0) {
      const bool trailing_only = stars == 1 && w.text.back() == '*' && !w.literal_at(w.text.size() - 1);
      if (trailing_only && w.text.size() > 1) {
        w.text.pop_back();
        mode = TermMode::kPrefix;
      } else if (header_field(f) && w.text.size() > stars) {
        mode = TermMode::kWildcard;
      } else if (w.text.size() == stars) {
        hint(HintKind::kIgnored, begin, pos_);
        return std::nullopt;
      } else {
        hint(HintKind::kTreatedAsText, begin, pos_);  // infix wildcards only in header fields
      }
    }
    return term(std::move(w.text), f, mode, span_from(begin));
  }

  // Up to 16 plain words ahead (no escapes, colons, operators, prefixes or structure).
  std::optional<uint32_t> bare_date(size_t begin) {
    std::vector<std::string_view> words;
    std::vector<size_t> ends;
    size_t p = pos_;
    while (words.size() < 16) {
      while (p < s_.size() && is_ws(s_[p])) ++p;
      if (p >= s_.size() || is_structural(s_[p]) || s_[p] == '-' || s_[p] == '+' || op_word_at(p, "OR") ||
          op_word_at(p, "AND") || op_word_at(p, "AROUND")) {
        break;
      }
      size_t e = p;
      bool plain = true;
      while (e < s_.size() && !is_ws(s_[e]) && !is_structural(s_[e])) {
        if (s_[e] == '\\' || s_[e] == ':' || s_[e] == '*') plain = false;
        ++e;
      }
      if (!plain) break;
      words.push_back(s_.substr(p, e - p));
      ends.push_back(e);
      p = e;
    }
    Period period;
    const size_t n = words.empty() ? 0 : match_date_phrase(words, o_.dates, &period);
    if (n == 0) return std::nullopt;
    std::string source;
    for (size_t i = 0; i < n; ++i) {
      if (i) source.push_back(' ');
      source.append(words[i]);
    }
    pos_ = ends[n - 1];
    Node d;
    d.kind = NodeKind::kDateRange;
    d.date_field = DateField::kReceived;
    d.date_op = DateOp::kOn;
    d.from = period.lo;
    d.to = period.hi;
    d.source_text = std::move(source);
    return add(std::move(d), span_from(begin));
  }

  std::vector<Item> value_items() {
    std::vector<Item> items;
    while (pos_ < s_.size() && items.size() < kMaxListItems) {
      items.push_back(s_[pos_] == '"' ? read_quoted() : read_word(true));
      if (pos_ + 1 < s_.size() && s_[pos_] == ',' && !is_ws(s_[pos_ + 1]) && s_[pos_ + 1] != ')' && s_[pos_ + 1] != '}') {
        ++pos_;
        continue;
      }
      break;
    }
    if (pos_ < s_.size() && s_[pos_] == ',') ++pos_;  // trailing comma: dropped
    if (items.size() == kMaxListItems && pos_ < s_.size() && !is_ws(s_[pos_]) && !is_structural(s_[pos_])) {
      const size_t rest = pos_;
      while (pos_ < s_.size() && !is_ws(s_[pos_]) && !is_structural(s_[pos_])) ++pos_;
      hint(HintKind::kTruncated, rest, pos_);
    }
    return items;
  }

  // `name:value…` at `begin`. nullopt = not a keyword (the caller reads a plain word).
  std::optional<std::optional<uint32_t>> keyword(size_t begin) {
    size_t p = pos_;
    if (p >= s_.size() || !is_alpha(s_[p])) return std::nullopt;
    while (p < s_.size() && ((s_[p] >= 'a' && s_[p] <= 'z') || (s_[p] >= 'A' && s_[p] <= 'Z') || (s_[p] >= '0' && s_[p] <= '9') ||
                             s_[p] == '_' || s_[p] == '-')) {
      ++p;
    }
    if (p >= s_.size() || s_[p] != ':') return std::nullopt;
    const std::string typed(s_.substr(pos_, p - pos_));
    const std::string name = lower_ascii(typed);
    const size_t vstart = p + 1;
    // A space (or nothing) after the colon makes the keyword literal text.
    if (vstart >= s_.size() || is_ws(s_[vstart]) || s_[vstart] == ')' || s_[vstart] == '}') return std::nullopt;
    const Keyword* kw = find_keyword(name);
    if (kw == nullptr) {
      if (s_[vstart] == '/') return std::nullopt;  // URLs (`https://…`) are plain text
      Item w = read_word(false);
      hint(HintKind::kTreatedAsText, begin, pos_);
      return bare_term(std::move(w), Field::kText, begin);
    }
    pos_ = vstart;
    bool negate = false;
    if (s_[pos_] == '-' && !ends_word(pos_ + 1) && s_[pos_ + 1] != ',') {
      negate = true;
      ++pos_;
    }
    const std::vector<Item> items = value_items();
    text_fallback_ = false;
    std::optional<uint32_t> node = build_keyword(*kw, typed, items, begin);
    if (node && negate && !text_fallback_) {
      Node n;
      n.kind = NodeKind::kNot;
      n.children = {*node};
      node = add(std::move(n), span_from(begin));
    }
    return node;
  }

  // The whole keyword token as free text, when its value is not valid for the keyword.
  std::optional<uint32_t> as_text(const std::vector<Item>& items, size_t begin) {
    text_fallback_ = true;
    hint(HintKind::kTreatedAsText, begin, pos_);
    const std::string_view raw = s_.substr(begin, pos_ - begin);
    if (items.size() == 1 && items[0].quoted) {
      if (items[0].text.empty()) return std::nullopt;
      return term(items[0].text, Field::kText, TermMode::kPhrase, span_from(begin));
    }
    std::string text;
    for (char ch : raw) {
      if (ch != '\\' && ch != '"') text.push_back(ch);
    }
    if (text.find(' ') != std::string::npos || text.find('\t') != std::string::npos) text = collapse_ws(text);
    if (text.find(' ') != std::string::npos) return term(std::move(text), Field::kText, TermMode::kPhrase, span_from(begin));
    return term(std::move(text), Field::kText, TermMode::kDefault, span_from(begin));
  }

  std::optional<uint32_t> filter(FilterKey k, std::string value, size_t begin) {
    Node n;
    n.kind = NodeKind::kFilter;
    n.key = k;
    n.value = std::move(value);
    return add(std::move(n), span_from(begin));
  }

  std::optional<std::pair<FilterKey, std::string>> filter_value(const Keyword& kw, const std::string& typed_name, const Item& it) {
    const std::string& v = it.text;
    const std::string l = lower_ascii(v);
    switch (kw.kind) {
      case KwKind::kHas: {
        if (l == "attachment") return std::pair{FilterKey::kAttachment, std::string("yes")};
        for (std::string_view t : {"pdf", "image", "document", "spreadsheet", "presentation", "archive", "calendar", "link"}) {
          if (l == t) return std::pair{FilterKey::kHas, l};
        }
        return std::nullopt;
      }
      case KwKind::kIs: {
        if (l == "read") return std::pair{FilterKey::kRead, std::string("yes")};
        if (l == "unread") return std::pair{FilterKey::kRead, std::string("no")};
        if (l == "flagged" || l == "starred") return std::pair{FilterKey::kFlagged, std::string("yes")};
        if (l == "encrypted") return std::pair{FilterKey::kEncrypted, std::string("yes")};
        if (l == "signed") return std::pair{FilterKey::kSigned, std::string("yes")};
        if (l == "sent" || l == "draft" || l == "replied" || l == "bulk") return std::pair{FilterKey::kIs, l};
        if (l == "muted" || l == "important") return std::pair{FilterKey::kUnavailable, lower_ascii(typed_name) + ":" + v};
        return std::nullopt;
      }
      case KwKind::kFilter: break;
      default: return std::nullopt;
    }
    switch (kw.key) {
      case FilterKey::kAttachment:
      case FilterKey::kRead:
      case FilterKey::kFlagged:
      case FilterKey::kSigned:
      case FilterKey::kEncrypted: {
        auto yn = yes_no(v);
        if (!yn) return std::nullopt;
        return std::pair{kw.key, *yn};
      }
      case FilterKey::kImportance: {
        if (l == "low" || l == "0") return std::pair{kw.key, std::string("low")};
        if (l == "medium" || l == "normal" || l == "1") return std::pair{kw.key, std::string("medium")};
        if (l == "high" || l == "2") return std::pair{kw.key, std::string("high")};
        return std::nullopt;
      }
      case FilterKey::kFolder: {
        if (v.empty()) return std::nullopt;
        if (l == "snoozed") return std::pair{FilterKey::kUnavailable, lower_ascii(typed_name) + ":" + v};
        if (l == "spam") return std::pair{kw.key, std::string("junk")};
        for (std::string_view r : {"inbox", "sent", "drafts", "archive", "trash", "junk", "anywhere"}) {
          if (l == r) return std::pair{kw.key, l};
        }
        return std::pair{kw.key, v};
      }
      case FilterKey::kLarger:
      case FilterKey::kSmaller: {
        auto b = size_bytes(v);
        if (!b) return std::nullopt;
        return std::pair{kw.key, *b};
      }
      case FilterKey::kMsgId: {
        std::string id = v;
        if (id.size() >= 2 && id.front() == '<' && id.back() == '>') id = id.substr(1, id.size() - 2);
        if (id.empty() || id.find(' ') != std::string::npos) return std::nullopt;
        return std::pair{kw.key, id};
      }
      default:
        if (v.empty()) return std::nullopt;
        return std::pair{kw.key, v};
    }
  }

  std::optional<uint32_t> build_keyword(const Keyword& kw, const std::string& typed_name, const std::vector<Item>& items, size_t begin) {
    if (items.empty()) return as_text(items, begin);
    std::vector<uint32_t> alts;
    switch (kw.kind) {
      case KwKind::kUnavailable: {
        std::string value(kw.name);
        value.push_back(':');
        for (size_t i = 0; i < items.size(); ++i) {
          if (i) value.push_back(',');
          value += items[i].text;
        }
        hint(HintKind::kNotAvailable, begin, pos_);
        return filter(FilterKey::kUnavailable, std::move(value), begin);
      }
      case KwKind::kDate: {
        std::vector<Node> ranges;
        for (const Item& it : items) {
          auto p = parse_date_value(it.text, o_.dates);
          if (!p) return as_text(items, begin);
          const Period r = apply_date_op(kw.date_op, *p);
          Node d;
          d.kind = NodeKind::kDateRange;
          d.date_field = kw.date_field;
          d.date_op = kw.date_op;
          d.from = r.lo;
          d.to = r.hi;
          d.source_text = collapse_ws(it.text);
          ranges.push_back(std::move(d));
        }
        for (Node& d : ranges) {
          if (auto id = add(std::move(d), span_from(begin))) alts.push_back(*id);
        }
        break;
      }
      case KwKind::kFilter:
      case KwKind::kHas:
      case KwKind::kIs: {
        std::vector<std::pair<FilterKey, std::string>> values;
        for (const Item& it : items) {
          auto fv = filter_value(kw, typed_name, it);
          if (!fv) return as_text(items, begin);
          values.push_back(std::move(*fv));
        }
        for (auto& [k, v] : values) {
          if (k == FilterKey::kUnavailable) hint(HintKind::kNotAvailable, begin, pos_);
          if (auto id = filter(k, std::move(v), begin)) alts.push_back(*id);
        }
        break;
      }
      case KwKind::kText: {
        const uint32_t roles = person_roles(kw.field);
        std::vector<std::string> addresses;
        std::vector<size_t> address_items;
        for (size_t i = 0; i < items.size(); ++i) {
          const Item& it = items[i];
          if (roles != 0 && !it.quoted && it.text.size() > 2 && it.text.front() == '<' && it.text.back() == '>' && !it.literal_at(0) &&
              it.text.find(' ') == std::string::npos) {
            addresses.push_back(lower_ascii(it.text.substr(1, it.text.size() - 2)));
            address_items.push_back(i);
          }
        }
        const bool one_person = !addresses.empty() && addresses.size() == items.size();
        if (one_person) {
          Node n;
          n.kind = NodeKind::kPerson;
          n.addresses = std::move(addresses);
          n.roles = roles;
          return add(std::move(n), span_from(begin));
        }
        size_t next_address = 0;
        for (size_t i = 0; i < items.size(); ++i) {
          const Item& it = items[i];
          if (next_address < address_items.size() && address_items[next_address] == i) {
            Node n;
            n.kind = NodeKind::kPerson;
            n.addresses = {addresses[next_address++]};
            n.roles = roles;
            if (auto id = add(std::move(n), span_from(begin))) alts.push_back(*id);
            continue;
          }
          if (roles != 0 && !it.quoted && lower_ascii(it.text) == "me" && it.esc.find('\1') == std::string::npos) {
            Node n;
            n.kind = NodeKind::kPerson;
            n.me = true;
            n.roles = roles;
            if (auto id = add(std::move(n), span_from(begin))) alts.push_back(*id);
            continue;
          }
          if (it.quoted) {
            if (it.text.empty()) continue;
            if (auto id = term(it.text, kw.field, TermMode::kPhrase, span_from(begin))) alts.push_back(*id);
            continue;
          }
          if (!it.text.empty() && it.text[0] == '+' && !it.literal_at(0)) {
            if (it.text.size() == 1) continue;
            if (auto id = term(it.text.substr(1), kw.field, TermMode::kExact, span_from(begin))) alts.push_back(*id);
            continue;
          }
          if (auto id = bare_term(it, kw.field, begin)) alts.push_back(*id);
        }
        break;
      }
    }
    if (alts.empty()) {
      hint(HintKind::kIgnored, begin, pos_);
      return std::nullopt;
    }
    return group(alts, NodeKind::kOr, span_from(begin));
  }

  std::string_view s_;
  const ParseOptions& o_;
  ParseResult& out_;
  size_t pos_ = 0;
  bool full_ = false;
  bool text_fallback_ = false;  // build_keyword degraded the token to text (no Not around it)
  size_t leaves_ = 0;           // nodes other than And/Or, bounded by kMaxNodes
};

// ------------------------------------------------------------------------------ printing

class Printer {
 public:
  Printer(const Query& q, const ParseOptions& o) : q_(q), o_(o) {}

  std::string print(uint32_t id, NodeKind parent, bool top) {
    const Node& n = q_.node(id);
    switch (n.kind) {
      case NodeKind::kAnd: {
        std::string body = join(n.children, NodeKind::kAnd, " ");
        return top ? body : "(" + body + ")";
      }
      case NodeKind::kOr: {
        std::string body = join(n.children, NodeKind::kOr, " OR ");
        return (parent == NodeKind::kOr || parent == NodeKind::kNot) ? "(" + body + ")" : body;
      }
      case NodeKind::kNot: return "-" + print(n.children[0], NodeKind::kNot, false);
      case NodeKind::kTerm: return term(n);
      case NodeKind::kPerson: {
        std::string out(person_keyword(n.roles));
        out.push_back(':');
        if (n.me) return out + "me";
        for (size_t i = 0; i < n.addresses.size(); ++i) {
          if (i) out.push_back(',');
          out += "<" + n.addresses[i] + ">";
        }
        return out;
      }
      case NodeKind::kFilter: {
        if (n.key == FilterKey::kUnavailable) {
          const size_t c = n.value.find(':');
          if (c == std::string::npos) return value_text(n.value);
          return n.value.substr(0, c + 1) + value_text(n.value.substr(c + 1));
        }
        return std::string(filter_name(n.key)) + ":" + value_text(n.value);
      }
      case NodeKind::kDateRange: return std::string(date_name(n.date_field, n.date_op)) + ":" + value_text(n.source_text);
    }
    return {};
  }

 private:
  std::string join(const std::vector<uint32_t>& ids, NodeKind kind, std::string_view sep) {
    std::string out;
    for (size_t i = 0; i < ids.size(); ++i) {
      if (i) out.append(sep);
      out += print(ids[i], kind, false);
    }
    return out;
  }

  std::string term(const Node& n) {
    const bool bare = n.field == Field::kText;
    const Ctx ctx = bare ? Ctx::kBare : Ctx::kValue;
    std::string out = bare ? std::string() : std::string(field_name(n.field)) + ":";
    switch (n.mode) {
      case TermMode::kDefault: return out + escape_word(n.text, ctx, false, true, o_);
      case TermMode::kExact: return out + "+" + escape_word(n.text, ctx, false, false, o_);
      case TermMode::kPrefix: return out + escape_word(n.text, ctx, false, false, o_) + "*";
      case TermMode::kPhrase: return out + quote(n.text);
      case TermMode::kWildcard: return out + escape_word(n.text, ctx, true, false, o_);
      case TermMode::kNear: {
        const size_t sp = n.text.find(' ');
        const std::string_view a = std::string_view(n.text).substr(0, sp);
        const std::string_view b = sp == std::string::npos ? std::string_view() : std::string_view(n.text).substr(sp + 1);
        return out + escape_word(a, Ctx::kBare, false, true, o_) + " AROUND " + std::to_string(n.near) + " " +
               escape_word(b, Ctx::kBare, false, false, o_);
      }
    }
    return out;
  }

  const Query& q_;
  const ParseOptions& o_;
};

bool same_node(const Query& qa, uint32_t a, const Query& qb, uint32_t b, size_t depth) {
  if (depth > kMaxNodes) return false;
  const Node& x = qa.node(a);
  const Node& y = qb.node(b);
  if (x.kind != y.kind || x.children.size() != y.children.size()) return false;
  switch (x.kind) {
    case NodeKind::kTerm:
      if (x.text != y.text || x.field != y.field || x.mode != y.mode || x.near != y.near || x.script != y.script) return false;
      break;
    case NodeKind::kPerson:
      if (x.addresses != y.addresses || x.roles != y.roles || x.me != y.me) return false;
      break;
    case NodeKind::kFilter:
      if (x.key != y.key || x.value != y.value) return false;
      break;
    case NodeKind::kDateRange:
      if (x.date_field != y.date_field || x.date_op != y.date_op || x.from != y.from || x.to != y.to || x.source_text != y.source_text) {
        return false;
      }
      break;
    default: break;
  }
  for (size_t i = 0; i < x.children.size(); ++i) {
    if (!same_node(qa, x.children[i], qb, y.children[i], depth + 1)) return false;
  }
  return true;
}

// Copies the subtree at `id` into `out` without `skip`; groups left with one child collapse.
std::optional<uint32_t> copy_without(const Query& q, uint32_t id, uint32_t skip, Query& out) {
  if (id == skip) return std::nullopt;
  Node n = q.node(id);
  if (n.kind == NodeKind::kAnd || n.kind == NodeKind::kOr || n.kind == NodeKind::kNot) {
    std::vector<uint32_t> kids;
    for (uint32_t c : n.children) {
      if (auto k = copy_without(q, c, skip, out)) kids.push_back(*k);
    }
    if (kids.empty()) return std::nullopt;
    if (n.kind != NodeKind::kNot && kids.size() == 1) return kids[0];
    n.children = std::move(kids);
  }
  out.nodes.push_back(std::move(n));
  return static_cast<uint32_t>(out.nodes.size() - 1);
}

}  // namespace

bool is_keyword(std::string_view lowercase_name) { return find_keyword(lowercase_name) != nullptr; }

std::vector<std::string_view> keyword_completions(std::string_view prefix) {
  const std::string p = lower_ascii(prefix);
  static const std::vector<std::string> kNames = [] {
    std::vector<std::string> v;
    v.reserve(kKeywords.size());
    for (const Keyword& k : kKeywords) v.push_back(std::string(display_name(k.name)) + ":");
    return v;
  }();
  std::vector<std::string_view> out;
  for (const std::string& n : kNames) {
    if (lower_ascii(n).starts_with(p)) out.emplace_back(n);
  }
  return out;
}

ParseResult parse_query(std::string_view text, const ParseOptions& options, std::string* sanitized) {
  ParseResult out;
  bool truncated = false;
  const std::string clean = sanitize(text, &truncated);
  Parser(clean, options, out).run();
  if (truncated) {
    out.hints.push_back({HintKind::kTruncated, {static_cast<uint32_t>(clean.size()), static_cast<uint32_t>(clean.size())}});
  }
  if (sanitized != nullptr) *sanitized = clean;
  return out;
}

std::string canonical_text(const Query& q, const ParseOptions& options) {
  if (!q.root) return {};
  return Printer(q, options).print(*q.root, NodeKind::kAnd, true);
}

bool same_query(const Query& a, const Query& b) {
  if (a.scope != b.scope || a.sort != b.sort || a.hint != b.hint) return false;
  if (!a.root || !b.root) return !a.root && !b.root;
  return same_node(a, *a.root, b, *b.root, 0);
}

Query remove_chip(const Query& q, uint32_t node) {
  Query out;
  out.scope = q.scope;
  out.sort = q.sort;
  out.hint = q.hint;
  if (!q.root || node >= q.nodes.size()) {
    out.nodes = q.nodes;
    out.root = q.root;
    return out;
  }
  const Node& removed = q.node(node);
  if (removed.kind == NodeKind::kDateRange) out.hint = DateHint{removed.from, removed.to};
  out.root = copy_without(q, *q.root, node, out);
  return out;
}

std::string describe_node(const Query& q, uint32_t node) {
  if (node >= q.nodes.size()) return {};
  const Node& n = q.node(node);
  switch (n.kind) {
    case NodeKind::kAnd: return "all of";
    case NodeKind::kOr: return "any of";
    case NodeKind::kNot: return "not " + describe_node(q, n.children[0]);
    case NodeKind::kTerm: return n.field == Field::kText ? n.text : std::string(field_name(n.field)) + ": " + n.text;
    case NodeKind::kPerson: {
      std::string out = std::string(person_keyword(n.roles)) + ": ";
      if (n.me) return out + "me";
      for (size_t i = 0; i < n.addresses.size(); ++i) out += (i ? ", " : "") + n.addresses[i];
      return out;
    }
    case NodeKind::kFilter:
      switch (n.key) {
        case FilterKey::kUnavailable: return n.value + " \u2014 not available";
        case FilterKey::kRead: return n.value == "yes" ? "is read" : "is unread";
        case FilterKey::kFlagged: return n.value == "yes" ? "is flagged" : "is not flagged";
        case FilterKey::kAttachment: return n.value == "yes" ? "has attachment" : "no attachment";
        case FilterKey::kSigned: return n.value == "yes" ? "is signed" : "is not signed";
        case FilterKey::kEncrypted: return n.value == "yes" ? "is encrypted" : "is not encrypted";
        case FilterKey::kIs: return "is " + n.value;
        case FilterKey::kHas: return "has " + n.value;
        case FilterKey::kLarger: return "larger than " + n.value + " bytes";
        case FilterKey::kSmaller: return "smaller than " + n.value + " bytes";
        case FilterKey::kFolder: return n.value == "anywhere" ? "all folders incl. Trash and Junk" : "in: " + n.value;
        default: return std::string(filter_name(n.key)) + ": " + n.value;
      }
    case NodeKind::kDateRange: {
      std::string out = describe_range(n.from, n.to);
      return n.date_field == DateField::kSent ? "sent " + out : out;
    }
  }
  return {};
}

}  // namespace mailsearch

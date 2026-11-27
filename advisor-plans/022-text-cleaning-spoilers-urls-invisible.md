# Plan 022: Stop spoken spoilers, URLs and invisible-character blocklist bypasses

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-core/src/text_cleaning.rs`
> Any change → compare excerpts below; mismatch = STOP.

## Status

- **Priority**: P1 · **Effort**: S · **Risk**: LOW (pure function with tests) · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

`clean_text` decides what the bot reads aloud. Its own contract (`text_cleaning.rs:3`) is that
protected Discord content is never spoken. Three gaps break that: (1) spoilers are removed
**before** code, so a message that shows `||` inside inline code pairs that `||` with the real
spoiler's opener and the hidden text is spoken; (2) URLs are matched before Unicode
normalisation, so a full-width `ｈｔｔｐｓ://…` survives the URL filter and is then folded into
an ordinary URL and read out; (3) only ZWJ/VS16/keycap are stripped, so a server's blocked
word with a zero-width space or soft hyphen inside it no longer matches the blocklist
(which runs on this cleaned text) but looks identical in chat.

## Current state

`crates/vozen-core/src/text_cleaning.rs`:

- Lines 12-17: `RE_CODE_BLOCK = (?s)```.*?```` , `RE_INLINE_CODE = `[^`]*``, `RE_SPOILER = (?s)\|\|.*?\|\|`.
- Lines 46-50 (`clean_text`):
  ```rust
  let mut text = RE_SPOILER.replace_all(raw, " ").into_owned();
  text = RE_CODE_BLOCK.replace_all(&text, " ").into_owned();
  text = RE_INLINE_CODE.replace_all(&text, " ").into_owned();
  text = RE_URL.replace_all(&text, " ").into_owned();
  ```
  then mentions/emoji, `text.retain(|c| !is_emoji_component(c))` (line 70), `text = text.nfkc().collect();` (line 75).
- Lines 81-95 `collect_url_media` and 97-107 `collect_markdown_media` apply the same spoiler-first order
  ("in the same order as `clean_text`"); `collect_markdown_media` deliberately counts code nested inside a spoiler only once.
- Lines 127-130 `is_emoji_component`: `'\u{200d}' | '\u{fe0f}' | '\u{20e3}'` and regional indicators.
- Consumers: `crates/vozen-discord/src/speech_preparation.rs:118` and `:154`; the blocklist matcher
  `crates/vozen-core/src/speech_safety.rs:205-217, 269-275` matches escaped literals on the cleaned text.
- Existing tests: `text_cleaning.rs` `mod tests` from line 176 (6 tests, e.g. `removes_emoji_components_and_collapses_unicode_repetition` at 258).

## Scope

**In scope**: `crates/vozen-core/src/text_cleaning.rs` (code + tests).
**Out of scope**: `speech_safety.rs`, `speech_preparation.rs`, any locale/i18n file.

## Steps

### Step 1: Code spans win over spoilers

Add a private helper that **masks** code: for each `RE_CODE_BLOCK` then `RE_INLINE_CODE` match,
replace the matched bytes with the same number of ASCII spaces (keeps byte offsets valid). Find
spoiler ranges with `RE_SPOILER` on the masked text, then:

- `clean_text`: remove code blocks, inline code and those spoiler ranges (all become `" "`).
- `collect_url_media`: search URLs in text with code and spoilers removed.
- `collect_markdown_media`: spoilers = matches on the masked text; code = code matches that are not
  inside a spoiler range (preserves "nested code counted once").

### Step 2: Normalise before matching URLs

Apply `nfkc()` at the **start** of `clean_text` and `collect_url_media` (before any regex); keep the
later fold or remove it if now redundant, as long as `folds_decorative_unicode_letters_before_tts` still passes.

### Step 3: Strip invisible format characters

Extend the stripped set (rename `is_emoji_component` to something like `is_ignorable_component` if you
like) with: U+00AD, U+200B, U+200E, U+200F, U+202A–U+202E, U+2060–U+2064, U+FEFF. Do **not** strip
U+200C (ZWNJ is meaningful in Persian and Indic scripts). Strip them at the start, before the regexes.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-core text_cleaning --locked` → all pass.

## Test plan (add to `mod tests`, model after the tests at lines 176-300)

Exact inputs (Rust string literals):

```rust
// 1. Spoiler after inline code that shows the spoiler syntax.
let leak = "use `||` to hide, like ||ending||";
// clean_text(leak) contains "use" and "hide" and does NOT contain "ending".
// collect_markdown_media(leak) == one Spoiler + one Code.

// 2. Code nested inside a spoiler is still counted once (unchanged behaviour).
let nested = "||secret `x`||";
// collect_markdown_media(nested) == [MediaKind::Spoiler]

// 3. Full-width URL.
let wide = "see ｈｔｔｐｓ://example.com/x now";
// clean_text(wide) does NOT contain "example"; collect_url_media(wide) == [MediaKind::Link]

// 4. Invisible characters inside a word.
// clean_text("bad\u{200B}word") == clean_text("badword"); same for "\u{00AD}".
// clean_text("می\u{200C}خواهم") still contains '\u{200C}' (ZWNJ is kept).
```

5. The existing 6 tests still pass unchanged.

## Done criteria

- [ ] targeted tests pass incl. the new ones; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only `text_cleaning.rs` changed; one commit, e.g. `Keep spoilers, disguised links and hidden characters out of speech`

## STOP conditions

- A committed contract or fixture file (search `contracts/` and `crates/*/tests` for `clean_text` expectations) encodes the old spoiler-first output.
- An existing test fails and the fix would require changing its expected value — report the case instead of editing the expectation.

## Maintenance notes

- Any new Discord markdown rule (e.g. headers) should be added through the same masking helper so ordering stays in one place.

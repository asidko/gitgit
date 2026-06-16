//! Syntect wrapper: tokenize raw source into our [`Token`]s by language.
//!
//! We deliberately do NOT use a syntect theme (the bundled themes do not match
//! our palette). Instead we self-map the parsed scope stack to our semantic
//! [`TokenKind`], and the theme owns the colors. The [`SyntaxSet`] is loaded once
//! and shared via a `OnceLock`.
//!
//! This module is the only place that imports syntect. The fixtures call it when
//! building a [`crate::diff::FileView`]; the `ui` layer never sees syntect.

use std::sync::OnceLock;

use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

use crate::diff::{Token, TokenKind};

/// Shared, lazily-loaded syntax definitions. We use `two-face`'s extended set
/// (bat's syntaxes, newlines variant, fancy-regex engine to match our syntect
/// build) instead of syntect's small bundled defaults: the defaults lack TOML,
/// INI, Dockerfile, `.env` (DotENV) and `.gitignore` (Git Ignore), which read as
/// plain text without it.
static SYNTAX: OnceLock<SyntaxSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX.get_or_init(two_face::syntax::extra_newlines)
}

/// Resolve a language id (token or name, e.g. "go", "Rust") to a syntax, or
/// `None` when the language is unknown.
fn find_syntax<'a>(set: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    set.find_syntax_by_token(lang)
        .or_else(|| set.find_syntax_by_name(lang))
}

/// A language hint for `path`: its extension (`README.md` -> `md`,
/// `config.toml` -> `toml`), else its base name with a leading dot stripped so
/// dotfiles and extensionless files still resolve (`.gitignore` -> `gitignore`,
/// `.env` -> `env`, `Dockerfile` -> `Dockerfile`, `Makefile` -> `Makefile`). The
/// highlighter then matches this token against syntect's names + file extensions
/// (case-insensitive); an unknown token falls through to a single `Ident`. The one
/// home for path -> language so the read-only and live-edit highlight paths agree.
pub fn lang_of(path: &str) -> String {
    let p = std::path::Path::new(path);
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        return ext.to_string();
    }
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.trim_start_matches('.').to_string())
        .unwrap_or_default()
}

/// Tokenize one line of `text` in `lang`. Unknown language -> a single `Ident`
/// token spanning the whole line (never panics).
pub fn highlight_line(lang: &str, text: &str) -> Vec<Token> {
    let set = syntax_set();
    let Some(syntax) = find_syntax(set, lang) else {
        return single_ident(text);
    };
    let mut parser = ParseState::new(syntax);
    tokenize_line(set, &mut parser, &mut ScopeStack::new(), text)
}

/// Tokenize a whole `src` block (newline-separated) in `lang`, preserving parser
/// state across lines. Unknown language -> one `Ident` token per line.
pub fn highlight_block(lang: &str, src: &str) -> Vec<Vec<Token>> {
    let set = syntax_set();
    let Some(syntax) = find_syntax(set, lang) else {
        return src.lines().map(single_ident).collect();
    };
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    src.lines()
        .map(|line| tokenize_line(set, &mut parser, &mut stack, line))
        .collect()
}

/// Fallback for an unknown language or empty parse: the whole line as one ident.
fn single_ident(text: &str) -> Vec<Token> {
    if text.is_empty() {
        return Vec::new();
    }
    vec![Token {
        text: text.to_string(),
        kind: TokenKind::Ident,
    }]
}

/// Run one line through the parser, applying scope ops to `stack`, and collapse
/// adjacent same-kind runs into [`Token`]s. `parser`/`stack` carry over so block
/// highlighting stays correct across lines (e.g. multi-line strings/comments).
fn tokenize_line(
    set: &SyntaxSet,
    parser: &mut ParseState,
    stack: &mut ScopeStack,
    text: &str,
) -> Vec<Token> {
    let with_nl = format!("{text}\n");
    let ops = parser.parse_line(&with_nl, set).unwrap_or_default();

    let mut out: Vec<Token> = Vec::new();
    let mut last_end = 0usize;
    for (offset, op) in &ops {
        push_run(&mut out, &with_nl, last_end, *offset, stack);
        let _ = stack.apply(op);
        last_end = *offset;
    }
    // Trailing run up to (but excluding) the synthetic newline.
    let end = with_nl.len().saturating_sub(1);
    push_run(&mut out, &with_nl, last_end, end, stack);
    out
}

/// Emit a token for `src[start..end]` (byte range) classified by the current
/// scope `stack`, merging into the previous token when the kind matches.
fn push_run(out: &mut Vec<Token>, src: &str, start: usize, end: usize, stack: &ScopeStack) {
    if end <= start {
        return;
    }
    let text = &src[start..end];
    if text.is_empty() {
        return;
    }
    let kind = classify(stack);
    if let Some(last) = out.last_mut() {
        if last.kind == kind {
            last.text.push_str(text);
            return;
        }
    }
    out.push(Token {
        text: text.to_string(),
        kind,
    });
}

/// Map a scope stack to a [`TokenKind`] by scanning the most specific scope
/// first. The prefix table mirrors the consensus mapping.
fn classify(stack: &ScopeStack) -> TokenKind {
    for scope in stack.as_slice().iter().rev() {
        let name = scope.build_string();
        if let Some(kind) = kind_for_scope(&name) {
            return kind;
        }
    }
    TokenKind::Ident
}

/// Longest-meaningful-prefix scope classification. Ordered most-specific first so
/// `entity.name.function` beats a bare `entity.name`, etc. Covers code scopes plus
/// the markup scopes Markdown emits (headings/bold/links/code/lists) and the
/// config scopes TOML/INI/`.env` use, so those files are not flat `Ident` text.
fn kind_for_scope(scope: &str) -> Option<TokenKind> {
    const TABLE: &[(&str, TokenKind)] = &[
        ("comment", TokenKind::Comment),
        // Markdown / markup: code spans read as strings, headings + emphasis as
        // keywords, links as functions, list/quote bullets as comments.
        ("markup.raw", TokenKind::Str),
        ("markup.heading", TokenKind::Keyword),
        ("markup.bold", TokenKind::Keyword),
        ("markup.italic", TokenKind::Type),
        ("markup.underline.link", TokenKind::Func),
        ("markup.underline", TokenKind::Func),
        ("markup.link", TokenKind::Func),
        ("meta.link", TokenKind::Func),
        ("markup.list", TokenKind::Comment),
        ("markup.quote", TokenKind::Comment),
        ("string", TokenKind::Str),
        ("constant.numeric", TokenKind::Number),
        ("constant.language", TokenKind::Keyword),
        ("constant.character", TokenKind::Str),
        ("support.constant", TokenKind::Number),
        ("entity.name.function", TokenKind::Func),
        ("support.function", TokenKind::Func),
        ("entity.name.type", TokenKind::Type),
        // TOML/INI/HTML keys + sections: tags/sections read as types/keywords. INI
        // headings carry `entity.section.ini` (no `name`), so match that too.
        ("entity.name.tag", TokenKind::Type),
        ("entity.name.section", TokenKind::Keyword),
        ("entity.section", TokenKind::Keyword),
        ("entity.name", TokenKind::Type),
        ("support.type", TokenKind::Type),
        ("storage.type", TokenKind::Type),
        ("storage.modifier", TokenKind::Keyword),
        ("keyword.operator", TokenKind::Punct),
        ("keyword", TokenKind::Keyword),
        ("punctuation", TokenKind::Punct),
    ];
    TABLE
        .iter()
        .find(|(prefix, _)| scope.starts_with(prefix))
        .map(|(_, kind)| *kind)
}

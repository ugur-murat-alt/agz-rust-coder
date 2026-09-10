//! Bounded structural rewriting for migration edit planning.
//!
//! This is deliberately *not* a regular-expression or plain text replacement:
//! edits are located through a UTF-8 aware token scan that understands Rust
//! comments, string/char/raw literals, balanced delimiters, turbofish generics,
//! and nested call expressions. Every helper is total and returns `None` when
//! the surrounding syntax cannot be proven, so a caller can surface a typed
//! obligation instead of guessing a rewrite.

/// Byte span plus the token kind used by the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Ident,
    /// ASCII punctuation significant for structure (`,`, `!`, `:`, `<`, ...).
    Punct(u8),
    Open(u8),
    Close(u8),
    Literal,
    Comment,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
    /// Index of the matching open/close token, when the delimiter is balanced.
    pub pair: Option<usize>,
}

impl Token {
    pub(crate) fn is_punct(&self, value: u8) -> bool {
        self.kind == TokenKind::Punct(value)
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.kind, TokenKind::Open(_))
    }

    pub(crate) fn is_open_paren(&self) -> bool {
        self.kind == TokenKind::Open(b'(')
    }

    pub(crate) fn is_ident(&self) -> bool {
        self.kind == TokenKind::Ident
    }

    pub(crate) fn is_comment(&self) -> bool {
        self.kind == TokenKind::Comment
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedFile {
    pub tokens: Vec<Token>,
    /// True when every delimiter in the file had a match.
    pub balanced: bool,
}

/// Tokenizes a UTF-8 Rust source file. Comments and literals are single tokens;
/// malformed or unterminated constructs consume the remainder of the input.
pub(crate) fn tokenize(source: &str) -> ParsedFile {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        match byte {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                tokens.push(token(start, index, TokenKind::Comment));
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                index += 2;
                while index < bytes.len() && depth > 0 {
                    if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                        depth += 1;
                        index += 2;
                    } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                tokens.push(token(start, index, TokenKind::Comment));
            }
            b'"' => {
                index = scan_quoted(source, index).unwrap_or(bytes.len());
                tokens.push(token(start, index, TokenKind::Literal));
            }
            b'\'' => {
                index = scan_char_or_lifetime(source, index).unwrap_or(start + 1);
                tokens.push(token(start, index, TokenKind::Literal));
            }
            b'b' if bytes.get(index + 1) == Some(&b'"') => {
                index = scan_quoted(source, index + 1).unwrap_or(bytes.len());
                tokens.push(token(start, index, TokenKind::Literal));
            }
            b'b' if bytes.get(index + 1) == Some(&b'\'') => {
                index = scan_char_or_lifetime(source, index + 1).unwrap_or(start + 2);
                tokens.push(token(start, index, TokenKind::Literal));
            }
            b'r' | b'b' => {
                if let Some(end) = scan_raw_string(source, index) {
                    index = end;
                    tokens.push(token(start, index, TokenKind::Literal));
                } else {
                    index = scan_ident(source, index);
                    tokens.push(token(start, index, TokenKind::Ident));
                }
            }
            b'(' | b'[' | b'{' => {
                index += 1;
                tokens.push(token(start, index, TokenKind::Open(byte)));
            }
            b')' | b']' | b'}' => {
                index += 1;
                tokens.push(token(start, index, TokenKind::Close(byte)));
            }
            _ => {
                let character = source[index..].chars().next().unwrap_or(' ');
                if character == '_' || character.is_alphabetic() {
                    index = scan_ident(source, index);
                    tokens.push(token(start, index, TokenKind::Ident));
                } else if character.is_ascii_digit() {
                    index += character.len_utf8();
                    while index < bytes.len() {
                        let next = bytes[index];
                        if next.is_ascii_alphanumeric() || next == b'_' || next == b'.' {
                            index += 1;
                        } else {
                            break;
                        }
                    }
                    tokens.push(token(start, index, TokenKind::Other));
                } else if character.is_ascii_punctuation() {
                    index += character.len_utf8();
                    tokens.push(token(start, index, TokenKind::Punct(byte)));
                } else {
                    index += character.len_utf8();
                    tokens.push(token(start, index, TokenKind::Other));
                }
            }
        }
    }
    let balanced = pair_delimiters(&mut tokens);
    ParsedFile { tokens, balanced }
}

fn token(start: usize, end: usize, kind: TokenKind) -> Token {
    Token {
        start,
        end,
        kind,
        pair: None,
    }
}

fn scan_ident(source: &str, start: usize) -> usize {
    let Some(first) = source[start..].chars().next() else {
        return start;
    };
    if first != '_' && !first.is_alphabetic() {
        return start + first.len_utf8();
    }
    let base = start + first.len_utf8();
    let mut end = base;
    for (offset, character) in source[base..].char_indices() {
        if character == '_' || character.is_alphanumeric() {
            end = base + offset + character.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Scans a quoted literal beginning at `start` (the opening quote), honoring
/// backslash escapes. Returns the byte index just past the closing quote.
fn scan_quoted(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

/// Scans `'x'`, `'\n'`, or a lifetime `'a`. Returns the byte index just past
/// the token.
fn scan_char_or_lifetime(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let first = bytes.get(start + 1).copied()?;
    if first == b'\\' {
        let mut index = start + 2;
        while index < bytes.len() {
            if bytes[index] == b'\'' {
                return Some(index + 1);
            }
            index += 1;
        }
        return None;
    }
    let character = source[start + 1..].chars().next()?;
    if character == '_' || character.is_alphabetic() {
        let ident_end = scan_ident(source, start + 1);
        if bytes.get(ident_end) == Some(&b'\'') && ident_end == start + 2 {
            return Some(ident_end + 1);
        }
        return Some(ident_end);
    }
    let end = start + 1 + character.len_utf8();
    if bytes.get(end) == Some(&b'\'') {
        return Some(end + 1);
    }
    Some(end)
}

/// Recognizes raw string prefixes: `r"`, `r#"`, `br"`, `rb"`, ... A plain
/// `b"..."` byte string is handled by `scan_quoted` before this helper.
fn scan_raw_string(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut saw_r = false;
    let mut prefix_chars = 0usize;
    while index < bytes.len() && prefix_chars < 2 {
        match bytes[index] {
            b'r' => {
                saw_r = true;
                index += 1;
                prefix_chars += 1;
            }
            b'b' => {
                index += 1;
                prefix_chars += 1;
            }
            _ => break,
        }
    }
    if !saw_r {
        return None;
    }
    let mut hashes = 0usize;
    while bytes.get(index) == Some(&b'#') {
        hashes += 1;
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let mut closing = index + 1;
            let mut seen = 0usize;
            while seen < hashes && bytes.get(closing) == Some(&b'#') {
                closing += 1;
                seen += 1;
            }
            if seen == hashes {
                return Some(closing);
            }
        }
        index += 1;
    }
    None
}

fn pair_delimiters(tokens: &mut [Token]) -> bool {
    let mut stack: Vec<usize> = Vec::new();
    let mut balanced = true;
    for index in 0..tokens.len() {
        match tokens[index].kind {
            TokenKind::Open(_) => stack.push(index),
            TokenKind::Close(close) => {
                let Some(open_index) = stack.pop() else {
                    balanced = false;
                    continue;
                };
                let expected = match tokens[open_index].kind {
                    TokenKind::Open(b'(') => b')',
                    TokenKind::Open(b'[') => b']',
                    TokenKind::Open(b'{') => b'}',
                    _ => close,
                };
                if expected != close {
                    balanced = false;
                }
                tokens[open_index].pair = Some(index);
                tokens[index].pair = Some(open_index);
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        balanced = false;
    }
    balanced
}

/// Index of the non-trivia token containing `offset`.
pub(crate) fn token_at_offset(tokens: &[Token], offset: usize) -> Option<usize> {
    tokens
        .iter()
        .position(|token| token.start <= offset && offset < token.end && !token.is_comment())
}

pub(crate) fn next_significant(tokens: &[Token], index: usize) -> Option<usize> {
    let mut index = index.saturating_add(1);
    while index < tokens.len() {
        if !tokens[index].is_comment() {
            return Some(index);
        }
        index += 1;
    }
    None
}

pub(crate) fn prev_significant(tokens: &[Token], index: usize) -> Option<usize> {
    let mut index = index;
    while index > 0 {
        index -= 1;
        if !tokens[index].is_comment() {
            return Some(index);
        }
    }
    None
}

/// Open-delimiter token indexes that enclose `offset`, outermost first.
pub(crate) fn ancestors(tokens: &[Token], offset: usize) -> Vec<usize> {
    let mut result = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.is_open() {
            if let Some(close) = token.pair {
                if token.start < offset && offset < tokens[close].start {
                    result.push(index);
                }
            }
        }
    }
    result
}

/// True when `offset` is inside the argument list of a `name!(...)` invocation.
pub(crate) fn inside_macro(tokens: &[Token], offset: usize) -> bool {
    ancestors(tokens, offset).into_iter().any(|open| {
        prev_significant(tokens, open).is_some_and(|previous| tokens[previous].is_punct(b'!'))
    })
}

/// True when the `use` token at `use_index` is part of a `pub` / `pub(...)`
/// re-export rather than a private import.
pub(crate) fn use_is_public(source: &str, tokens: &[Token], use_index: usize) -> bool {
    let Some(previous) = prev_significant(tokens, use_index) else {
        return false;
    };
    let token = &tokens[previous];
    if token.is_ident() {
        return source.get(token.start..token.end) == Some("pub");
    }
    if matches!(token.kind, TokenKind::Close(b')')) {
        let Some(open) = token.pair else {
            return false;
        };
        let Some(before) = prev_significant(tokens, open) else {
            return false;
        };
        return tokens[before].is_ident()
            && source.get(tokens[before].start..tokens[before].end) == Some("pub");
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallShape {
    /// A regular `path(...)` / `path::<T>(...)` call.
    Call { open: usize, close: usize },
    /// `name!(...)`: a macro invocation cannot be rewritten by this engine.
    Macro,
    /// The identifier is referenced as a value rather than called.
    Value,
}

/// Resolves the argument list of the call whose callee identifier is
/// `identifier`. Only calls that directly parenthesize the (optionally
/// turbofished) identifier are accepted.
pub(crate) fn call_shape(parsed: &ParsedFile, identifier: usize) -> Option<CallShape> {
    let next = next_significant(&parsed.tokens, identifier)?;
    let token = &parsed.tokens[next];
    if token.is_open_paren() {
        return Some(CallShape::Call {
            open: next,
            close: tokens_pair(parsed, next)?,
        });
    }
    if token.is_punct(b'!') {
        return Some(CallShape::Macro);
    }
    if token.is_punct(b':') {
        let mut cursor = next;
        while parsed
            .tokens
            .get(cursor)
            .is_some_and(|token| token.is_punct(b':'))
        {
            cursor = next_significant(&parsed.tokens, cursor)?;
        }
        if parsed
            .tokens
            .get(cursor)
            .is_some_and(|token| token.is_punct(b'<'))
        {
            let close = skip_angle(parsed, cursor)?;
            let after = next_significant(&parsed.tokens, close)?;
            if parsed.tokens[after].is_open_paren() {
                return Some(CallShape::Call {
                    open: after,
                    close: tokens_pair(parsed, after)?,
                });
            }
        }
        return Some(CallShape::Value);
    }
    Some(CallShape::Value)
}

fn tokens_pair(parsed: &ParsedFile, index: usize) -> Option<usize> {
    if !parsed.balanced {
        return None;
    }
    parsed.tokens[index].pair
}

/// Finds the `<` block matching the opener at `open`, skipping `->`, `=>`,
/// `<=`, `>=` two-character operators. Returns `None` when the block is not
/// proven to close before a statement boundary.
pub(crate) fn skip_angle(parsed: &ParsedFile, open: usize) -> Option<usize> {
    let tokens = &parsed.tokens;
    if !tokens.get(open).is_some_and(|token| token.is_punct(b'<')) {
        return None;
    }
    let mut depth = 0usize;
    let mut cursor = open;
    while cursor < tokens.len() {
        let token = &tokens[cursor];
        if (token.is_punct(b'-') || token.is_punct(b'='))
            && next_significant(tokens, cursor).is_some_and(|next| tokens[next].is_punct(b'>'))
        {
            cursor += 2;
            continue;
        }
        if token.is_punct(b'<') {
            depth += 1;
        } else if token.is_punct(b'>') {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(cursor);
            }
        } else if depth > 0 && (token.is_punct(b';') || token.kind == TokenKind::Open(b'{')) {
            return None;
        }
        cursor += 1;
    }
    None
}

/// Signature parameter list resolved from a `fn` name token.
#[derive(Debug, Clone)]
pub(crate) struct ParamList {
    pub open: usize,
    pub close: usize,
    /// Parameter spans in written order, `self` receiver included.
    pub params: Vec<(usize, usize)>,
    pub receiver: bool,
    /// Byte offset of the `fn` keyword token.
    pub fn_token: usize,
}

impl ParamList {
    pub(crate) fn explicit(&self) -> &[(usize, usize)] {
        let skip = usize::from(self.receiver);
        &self.params[skip.min(self.params.len())..]
    }
}

/// Resolves the parameter list for the function whose name token is
/// `name_token`.
pub(crate) fn signature_params(
    source: &str,
    parsed: &ParsedFile,
    name_token: usize,
) -> Option<ParamList> {
    let tokens = &parsed.tokens;
    if !parsed.balanced || !tokens[name_token].is_ident() {
        return None;
    }
    let mut fn_token = None;
    let mut cursor = name_token;
    while let Some(previous) = prev_significant(tokens, cursor) {
        let token = &tokens[previous];
        if token.is_punct(b';') || matches!(token.kind, TokenKind::Open(_) | TokenKind::Close(_)) {
            break;
        }
        if token.is_ident() && source.get(token.start..token.end) == Some("fn") {
            fn_token = Some(previous);
            break;
        }
        cursor = previous;
    }
    let fn_token = fn_token?;
    let mut cursor = next_significant(tokens, fn_token)?;
    // The function name may be followed by a generic parameter block.
    if tokens[cursor].is_ident() {
        cursor = next_significant(tokens, cursor)?;
    }
    if tokens[cursor].is_punct(b'<') {
        cursor = skip_angle(parsed, cursor)?;
        cursor = next_significant(tokens, cursor)?;
    }
    if !tokens[cursor].is_open_paren() {
        return None;
    }
    let close = tokens[cursor].pair?;
    let spans = split_top_level(parsed, cursor, close)?;
    let receiver = spans.first().is_some_and(|(start, end)| {
        let text = source.get(*start..*end).unwrap_or_default().trim();
        text == "self"
            || text.starts_with("self:")
            || text == "&self"
            || text == "&mut self"
            || text == "mut self"
            || text.starts_with("&'")
    });
    Some(ParamList {
        open: cursor,
        close,
        params: spans,
        receiver,
        fn_token,
    })
}

/// Splits the comma-separated items between `open` and `close` at top level.
/// Angle brackets are tracked so `HashMap<u32, String>` stays one item.
pub(crate) fn split_top_level(
    parsed: &ParsedFile,
    open: usize,
    close: usize,
) -> Option<Vec<(usize, usize)>> {
    let tokens = &parsed.tokens;
    if !parsed.balanced || open >= close || close >= tokens.len() {
        return None;
    }
    let mut items = Vec::new();
    let mut item_start: Option<usize> = None;
    let mut angle = 0usize;
    let mut cursor = open + 1;
    while cursor < close {
        let token = &tokens[cursor];
        if token.is_open() {
            if item_start.is_none() {
                item_start = Some(token.start);
            }
            let pair = token.pair?;
            cursor = pair;
        } else if token.is_punct(b'<') {
            if item_start.is_none() {
                item_start = Some(token.start);
            }
            angle += 1;
        } else if token.is_punct(b'>') && angle > 0 {
            angle -= 1;
        } else if token.is_punct(b',') && angle == 0 {
            if let Some(start) = item_start.take() {
                items.push((start, token.start));
            }
        } else if item_start.is_none() {
            item_start = Some(token.start);
        }
        cursor += 1;
    }
    if let Some(start) = item_start.take() {
        items.push((start, tokens[close].start));
    }
    items.retain(|(start, end)| start < end);
    Some(items)
}

/// One structural mutation: replace `source[start..end]` with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Inserts `text` at position `index` among the explicit parameters/arguments.
/// `receiver` marks a leading `self` parameter that is not part of the index.
pub(crate) fn insert_into_params(
    parsed: &ParsedFile,
    params: &[(usize, usize)],
    open: usize,
    close: usize,
    receiver: bool,
    index: usize,
    text: &str,
) -> Option<ListEdit> {
    let tokens = &parsed.tokens;
    let skip = usize::from(receiver);
    let explicit = params.get(skip..)?;
    if index > explicit.len() {
        return None;
    }
    if explicit.is_empty() {
        let close_offset = tokens.get(close)?.start;
        if receiver {
            let receiver_end = params.get(skip.saturating_sub(1)).map(|(_, end)| *end)?;
            return Some(ListEdit {
                start: receiver_end,
                end: receiver_end,
                text: format!(", {text}"),
            });
        }
        let _ = open;
        return Some(ListEdit {
            start: close_offset,
            end: close_offset,
            text: text.to_owned(),
        });
    }
    if index < explicit.len() {
        let start = explicit[index].0;
        return Some(ListEdit {
            start,
            end: start,
            text: format!("{text}, "),
        });
    }
    // Append after the last explicit parameter, honoring a trailing comma.
    let last = explicit.last().copied()?;
    let close_offset = tokens.get(close)?.start;
    if let Some(comma) = find_top_level_comma(parsed, last.1, close_offset) {
        return Some(ListEdit {
            start: comma + 1,
            end: comma + 1,
            text: format!(" {text},"),
        });
    }
    Some(ListEdit {
        start: last.1,
        end: last.1,
        text: format!(", {text}"),
    })
}

/// Replaces the explicit item at `index` with `text`.
pub(crate) fn replace_in_params(
    params: &[(usize, usize)],
    receiver: bool,
    index: usize,
    text: &str,
) -> Option<ListEdit> {
    let skip = usize::from(receiver);
    let (start, end) = *params.get(skip + index)?;
    Some(ListEdit {
        start,
        end,
        text: text.to_owned(),
    })
}

fn find_top_level_comma(parsed: &ParsedFile, from: usize, to: usize) -> Option<usize> {
    let tokens = &parsed.tokens;
    let mut cursor = tokens.iter().position(|token| token.start >= from)?;
    while cursor < tokens.len() && tokens[cursor].start < to {
        let token = &tokens[cursor];
        if token.is_punct(b',') {
            return Some(token.start);
        }
        if token.is_open() {
            cursor = token.pair?;
        }
        cursor += 1;
    }
    None
}

/// Text of the signature from `fn` through the end of the return type or
/// `where` clause, bounded for reporting.
pub(crate) fn signature_text(
    source: &str,
    parsed: &ParsedFile,
    list: &ParamList,
    max_chars: usize,
) -> Option<String> {
    let tokens = &parsed.tokens;
    let mut cursor = list.close + 1;
    let mut end = tokens
        .get(list.close)
        .map_or(source.len(), |token| token.end);
    let mut angle = 0usize;
    while cursor < tokens.len() {
        let token = &tokens[cursor];
        if token.is_open() || token.is_punct(b';') {
            break;
        }
        if token.is_ident() && source.get(token.start..token.end) == Some("where") && angle == 0 {
            break;
        }
        if token.is_punct(b'<') {
            angle += 1;
        } else if token.is_punct(b'>') && angle > 0 {
            angle -= 1;
        }
        end = token.end;
        cursor += 1;
    }
    let text = source.get(tokens[list.fn_token].start..end)?.trim();
    Some(bounded_chars(text, max_chars))
}

/// Applies one edit to an owned source copy. Returns `None` for a range that
/// is not a valid UTF-8 boundary or an inverted span.
pub(crate) fn apply_edit(source: &str, edit: &ListEdit) -> Option<String> {
    if edit.start > edit.end
        || edit.end > source.len()
        || !source.is_char_boundary(edit.start)
        || !source.is_char_boundary(edit.end)
    {
        return None;
    }
    let mut updated = source.to_owned();
    updated.replace_range(edit.start..edit.end, &edit.text);
    Some(updated)
}

pub(crate) fn bounded_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> ParsedFile {
        tokenize(source)
    }

    fn find_ident(parsed: &ParsedFile, source: &str, name: &str, occurrence: usize) -> usize {
        let mut seen = 0usize;
        for (index, token) in parsed.tokens.iter().enumerate() {
            if token.is_ident() && source.get(token.start..token.end) == Some(name) {
                seen += 1;
                if seen >= occurrence {
                    return index;
                }
            }
        }
        let dump = parsed
            .tokens
            .iter()
            .map(|token| format!("{:?}:{:?}", token.kind, source.get(token.start..token.end)))
            .collect::<Vec<_>>()
            .join(" ");
        panic!("identifier {name} occurrence {occurrence} not found in: {dump}");
    }

    fn call_span(parsed: &ParsedFile, source: &str, name: &str) -> (usize, usize) {
        match call_shape(parsed, find_ident(parsed, source, name, 1)) {
            Some(CallShape::Call { open, close }) => (open, close),
            other => panic!("unexpected call shape for {name}: {other:?}"),
        }
    }

    #[test]
    fn string_and_comment_literals_are_single_tokens() {
        let source = "fn f() { let a = \"(\"; /* ) */ let b = 'x'; }";
        let parsed = parsed(source);
        assert!(parsed.balanced);
        assert!(
            parsed
                .tokens
                .iter()
                .any(|token| token.kind == TokenKind::Comment)
        );
        assert!(
            parsed
                .tokens
                .iter()
                .any(|token| token.kind == TokenKind::Literal)
        );
    }

    #[test]
    fn call_shape_handles_turbofish_and_detects_macros() {
        let source = "foo::<u32, String>(a, b); bar(a); baz!(a); let f = foo;";
        let parsed = parsed(source);
        assert!(matches!(
            call_shape(&parsed, find_ident(&parsed, source, "foo", 1)),
            Some(CallShape::Call { .. })
        ));
        assert!(matches!(
            call_shape(&parsed, find_ident(&parsed, source, "bar", 1)),
            Some(CallShape::Call { .. })
        ));
        assert_eq!(
            call_shape(&parsed, find_ident(&parsed, source, "baz", 1)),
            Some(CallShape::Macro)
        );
        assert_eq!(
            call_shape(&parsed, find_ident(&parsed, source, "foo", 2)),
            Some(CallShape::Value)
        );
    }

    #[test]
    fn split_top_level_keeps_generic_commas_together() {
        let source = "call(HashMap::<u32, String>::new(), other)";
        let parsed = parsed(source);
        let (open, close) = call_span(&parsed, source, "call");
        let items = split_top_level(&parsed, open, close).expect("split");
        assert_eq!(items.len(), 2, "{items:?}");
    }

    #[test]
    fn signature_params_finds_explicit_parameters_and_receiver() {
        let source = "pub fn scale(&self, base: u32) -> u32 { base }";
        let parsed = parsed(source);
        let name = find_ident(&parsed, source, "scale", 1);
        let list = signature_params(source, &parsed, name).expect("params");
        assert!(list.receiver);
        assert_eq!(list.explicit().len(), 1);
        let edit = insert_into_params(
            &parsed,
            &list.params,
            list.open,
            list.close,
            true,
            1,
            "factor: u32",
        )
        .expect("insert");
        let updated = apply_edit(source, &edit).expect("apply");
        assert!(
            updated.contains("fn scale(&self, base: u32, factor: u32) -> u32"),
            "{updated}"
        );
    }

    #[test]
    fn insertion_respects_index_and_trailing_commas() {
        let source = "run(a, b,)";
        let parsed = parsed(source);
        let (open, close) = call_span(&parsed, source, "run");
        let params = split_top_level(&parsed, open, close).expect("args");
        let append = insert_into_params(&parsed, &params, open, close, false, params.len(), "c")
            .expect("append");
        let updated = apply_edit(source, &append).expect("apply");
        assert_eq!(updated, "run(a, b, c,)");
        let middle =
            insert_into_params(&parsed, &params, open, close, false, 1, "c").expect("middle");
        let updated = apply_edit(source, &middle).expect("apply");
        assert_eq!(updated, "run(a, c, b,)");
    }

    #[test]
    fn utf8_offsets_are_preserved() {
        let source = "pub fn render(prefix: &str, label: &str) {}\n// ünïcödé\n";
        let parsed = parsed(source);
        let name = find_ident(&parsed, source, "render", 1);
        let list = signature_params(source, &parsed, name).expect("params");
        let edit = insert_into_params(
            &parsed,
            &list.params,
            list.open,
            list.close,
            false,
            1,
            "suffix: &str",
        )
        .expect("insert");
        let updated = apply_edit(source, &edit).expect("apply");
        assert!(
            updated.contains("prefix: &str, suffix: &str, label: &str"),
            "{updated}"
        );
    }

    #[test]
    fn inside_macro_detects_nested_call_arguments() {
        let source = "assert_eq!(dial.scale(3), 3);";
        let outer = parsed(source);
        let scale = find_ident(&outer, source, "scale", 1);
        assert!(inside_macro(&outer.tokens, outer.tokens[scale].start));
        let plain = "let value = dial.scale(3);";
        let reparsed = parsed(plain);
        let scale = find_ident(&reparsed, plain, "scale", 1);
        assert!(!inside_macro(
            &reparsed.tokens,
            reparsed.tokens[scale].start
        ));
    }

    #[test]
    fn unbalanced_input_is_rejected() {
        let parsed = parsed("fn f( {}");
        assert!(!parsed.balanced);
    }
}

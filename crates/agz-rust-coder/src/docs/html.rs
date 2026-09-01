//! Bounded rustdoc HTML extraction without executing page content.

pub const DOCS_MAX_OUTPUT: usize = 6_000;
pub const DOCS_MAX_HTML_BYTES: usize = 8 * 1024 * 1024;

const ITEM_KINDS: &[&str] = &[
    "struct", "enum", "trait", "union", "fn", "type", "constant", "static", "macro", "mod",
];

pub fn symbol_page_candidates(symbol: &str) -> Vec<String> {
    let parts = safe_symbol_parts(symbol);
    let Some(parts) = parts else {
        return Vec::new();
    };
    let Some(leaf) = parts.last() else {
        return Vec::new();
    };
    let prefix = parts[..parts.len() - 1].join("/");
    let mut candidates = Vec::new();
    for kind in ITEM_KINDS {
        candidates.push(format_item_page(&prefix, kind, leaf));
    }
    candidates.push(format_leaf_page(&prefix, leaf));
    if !prefix.is_empty() {
        candidates.push(format!("{prefix}/index.html"));
    }
    candidates.push(format!("{leaf}/index.html"));
    candidates.sort();
    candidates.dedup();
    candidates
}

pub fn page_candidates(symbol: Option<&str>) -> Vec<String> {
    symbol
        .filter(|symbol| !symbol.trim().is_empty())
        .map_or_else(|| vec!["index.html".to_owned()], symbol_page_candidates)
}

pub fn package_folder_names(crate_name: &str) -> Vec<String> {
    let underscored = safe_segment(&crate_name.replace('-', "_"));
    let hyphenated = safe_segment(crate_name);
    if underscored == hyphenated {
        vec![underscored]
    } else {
        vec![underscored, hyphenated]
    }
}

pub fn is_safe_page_path(path: &str) -> bool {
    let trimmed = path.trim();
    !trimmed.is_empty()
        && path == trimmed
        && !trimmed.starts_with('/')
        && !trimmed.contains('\\')
        && trimmed.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

pub fn strip_rustdoc_html(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    let bounded = if html.len() > DOCS_MAX_HTML_BYTES {
        let mut end = DOCS_MAX_HTML_BYTES;
        while !html.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        &html[..end]
    } else {
        html
    };
    let blocks = extract_docblocks(bounded);
    let selected = if blocks.is_empty() {
        select_main_content(bounded).unwrap_or(bounded)
    } else {
        return finish_text(&blocks.join("\n"));
    };
    finish_text(selected)
}

pub fn extract_rustdoc_text(html: &str) -> String {
    strip_rustdoc_html(html)
}

pub fn decode_html_entities(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'&' {
            let character = value[index..].chars().next().unwrap_or_default();
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        let Some(end_offset) = value[index..].find(';') else {
            output.push('&');
            index += 1;
            continue;
        };
        let end = index + end_offset;
        let entity = &value[index + 1..end];
        let decoded = if let Some(hex) = entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .map(|character| character.to_string())
        } else if let Some(decimal) = entity.strip_prefix('#') {
            decimal
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|character| character.to_string())
        } else {
            named_entity(entity).map(str::to_owned)
        };
        if let Some(decoded) = decoded {
            output.push_str(&decoded);
            index = end + 1;
        } else {
            output.push('&');
            index += 1;
        }
    }
    output
}

fn format_item_page(prefix: &str, kind: &str, leaf: &str) -> String {
    if prefix.is_empty() {
        format!("{kind}.{leaf}.html")
    } else {
        format!("{prefix}/{kind}.{leaf}.html")
    }
}

fn format_leaf_page(prefix: &str, leaf: &str) -> String {
    if prefix.is_empty() {
        format!("{leaf}.html")
    } else {
        format!("{prefix}/{leaf}.html")
    }
}

fn safe_symbol_parts(symbol: &str) -> Option<Vec<&str>> {
    let parts = symbol
        .trim()
        .split("::")
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| !is_identifier(part)) {
        None
    } else {
        Some(parts)
    }
}

fn is_identifier(value: &str) -> bool {
    let value = value.strip_suffix('!').unwrap_or(value);
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn safe_segment(value: &str) -> String {
    let segment = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if segment.is_empty() {
        "_".to_owned()
    } else {
        segment
    }
}

fn extract_docblocks(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut blocks = Vec::new();
    let mut search = 0;
    while let Some(relative) = lower[search..].find("<div") {
        let opening = search + relative;
        let Some(open_end) = tag_end(&lower, opening) else {
            break;
        };
        let tag = &lower[opening..=open_end];
        if !is_open_div_tag(tag) || !has_class_token(tag, "docblock") {
            search = open_end + 1;
            continue;
        }
        let mut depth = 1usize;
        let mut cursor = open_end + 1;
        let mut close_start = None;
        while cursor < lower.len() {
            let next_open = lower[cursor..]
                .find("<div")
                .map(|offset| (cursor + offset, true));
            let next_close = lower[cursor..]
                .find("</div")
                .map(|offset| (cursor + offset, false));
            let Some((next, is_open)) = [next_open, next_close]
                .into_iter()
                .flatten()
                .min_by_key(|(offset, _)| *offset)
            else {
                break;
            };
            let Some(next_end) = tag_end(&lower, next) else {
                break;
            };
            let next_tag = &lower[next..=next_end];
            if is_open {
                if is_open_div_tag(next_tag) && !next_tag.trim_end().ends_with("/>") {
                    depth += 1;
                }
            } else if is_close_div_tag(next_tag) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close_start = Some(next);
                    search = next_end + 1;
                    break;
                }
            }
            cursor = next_end + 1;
        }
        let Some(close_start) = close_start else {
            break;
        };
        let content = &html[open_end + 1..close_start];
        if !content.trim().is_empty() {
            blocks.push(content.to_owned());
        }
    }
    blocks
}

fn select_main_content(html: &str) -> Option<&str> {
    let lower = html.to_ascii_lowercase();
    for tag in ["main", "body"] {
        let Some(opening) = lower.find(&format!("<{tag}")) else {
            continue;
        };
        let Some(open_end) = tag_end(&lower, opening) else {
            continue;
        };
        let Some(closing) = lower[open_end + 1..]
            .find(&format!("</{tag}"))
            .map(|offset| offset + open_end + 1)
        else {
            continue;
        };
        return Some(&html[open_end + 1..closing]);
    }
    None
}

fn has_class_token(tag: &str, class: &str) -> bool {
    let Some(class_start) = tag.find("class=") else {
        return false;
    };
    let rest = &tag[class_start + "class=".len()..];
    let rest = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''));
    let Some(rest) = rest else {
        return false;
    };
    let end = rest.find(['"', '\'']).unwrap_or(rest.len());
    rest[..end]
        .split_ascii_whitespace()
        .any(|token| token == class)
}

fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in html[start..].bytes().enumerate() {
        match quote {
            Some(expected) if byte == expected => quote = None,
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'>' => return Some(start + offset),
            _ => {}
        }
    }
    None
}

fn is_open_div_tag(tag: &str) -> bool {
    tag.starts_with("<div")
        && tag
            .as_bytes()
            .get(4)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'>' || *byte == b'/')
}

fn is_close_div_tag(tag: &str) -> bool {
    tag.starts_with("</div")
        && tag
            .as_bytes()
            .get(5)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'>')
}

fn finish_text(input: &str) -> String {
    let mut without_unsafe = String::with_capacity(input.len());
    let lower = input.to_ascii_lowercase();
    let mut index = 0;
    while index < input.len() {
        if input[index..].starts_with("<!--") {
            let end = lower[index + 4..]
                .find("-->")
                .map_or(input.len(), |offset| index + 4 + offset + 3);
            without_unsafe.push(' ');
            index = end;
            continue;
        }
        if input.as_bytes()[index] == b'<' {
            let Some(end) = tag_end(&lower, index) else {
                without_unsafe.push(' ');
                index += 1;
                continue;
            };
            let tag = &lower[index..=end];
            if let Some(name) = excluded_tag_name(tag) {
                if !tag.trim_end().ends_with("/>") && !tag.starts_with("</") {
                    let close = format!("</{name}");
                    if let Some(relative) = lower[end + 1..].find(&close) {
                        let close_start = end + 1 + relative;
                        let close_end =
                            tag_end(&lower, close_start).unwrap_or(lower.len().saturating_sub(1));
                        index = close_end + 1;
                        without_unsafe.push(' ');
                        continue;
                    }
                }
            }
            without_unsafe.push(' ');
            index = end + 1;
            continue;
        }
        let character = input[index..].chars().next().unwrap_or_default();
        without_unsafe.push(character);
        index += character.len_utf8();
    }
    let decoded = decode_html_entities(&without_unsafe);
    let decoded = decode_html_entities(&decoded);
    decoded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(DOCS_MAX_OUTPUT)
        .collect()
}

fn excluded_tag_name(tag: &str) -> Option<&'static str> {
    let name = tag.trim_start_matches('<').trim_start_matches('/');
    ["script", "style", "svg", "noscript", "template"]
        .into_iter()
        .find(|candidate| {
            name.starts_with(candidate)
                && name.as_bytes().get(candidate.len()).is_some_and(|byte| {
                    byte.is_ascii_whitespace() || *byte == b'>' || *byte == b'/'
                })
        })
}

fn named_entity(entity: &str) -> Option<&'static str> {
    Some(match entity.to_ascii_lowercase().as_str() {
        "amp" => "&",
        "apos" => "'",
        "bull" => "\u{2022}",
        "copy" => "\u{00a9}",
        "divide" => "\u{00f7}",
        "eacute" => "\u{00e9}",
        "ge" => "\u{2265}",
        "gt" => ">",
        "hellip" => "...",
        "le" => "\u{2264}",
        "ldquo" => "\"",
        "lsquo" => "'",
        "lt" => "<",
        "mdash" => "-",
        "nbsp" => " ",
        "ndash" => "-",
        "ne" => "\u{2260}",
        "plusmn" => "\u{00b1}",
        "quot" => "\"",
        "rdquo" => "\"",
        "reg" => "\u{00ae}",
        "rsquo" => "'",
        "times" => "\u{00d7}",
        "trade" => "\u{2122}",
        _ => return None,
    })
}

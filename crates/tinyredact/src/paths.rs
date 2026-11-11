//! Home-directory path redaction for text that may be persisted or logged.

/// Replace recognized home-directory prefixes with `<HOME>`, preserving the
/// path below the user's home directory and all surrounding text.
///
/// Recognizes `/home/<user>`, `/Users/<user>`, drive-qualified Windows
/// `Users` paths, UNC `Users` and `home` paths, and tilde paths (`~` or
/// `~<user>`). Matching is case-insensitive for the conventional directory
/// names. After one of these explicit home prefixes, the nonempty profile
/// component is redacted through its path separator, including arbitrary UTF-8
/// and symbols. When no descendant separator is present, whitespace and clear
/// closing punctuation end the component; terminal dots, commas and semicolons
/// remain surrounding text. A bare `~` is recognized at end of input or
/// before whitespace or a closing delimiter; `~.` and `~.name` are relative
/// paths and are left unchanged. Other absolute paths are left unchanged.
/// Windows drive and UNC profiles may contain spaces when a descendant
/// separator completes the path. A new absolute path after prose ends that
/// profile; quoted paths use the matching quote to bound a bare profile.
pub fn redact_home_paths(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut position = 0;

    while position < text.len() {
        if let Some(uri) = parse_uri(text, position) {
            if uri.is_file
                && let Some(path_start) = uri.path_start
                && let Some((match_start, end)) =
                    file_uri_home_path_end(&bytes[..uri.path_end], path_start)
            {
                output.push_str(&text[position..match_start]);
                if match_start == path_start {
                    output.push('/');
                }
                output.push_str("<HOME>");
                output.push_str(&text[end..uri.path_end]);
            } else {
                output.push_str(&text[position..uri.path_end]);
            }
            // A query or fragment (`?path=/Users/…`) is ordinary text: the
            // scan below still redacts a home path inside it.
            position = uri.path_end;
            continue;
        }

        if has_path_boundary(text, position)
            && let Some(consumed) = home_path_end(bytes, position)
        {
            output.push_str("<HOME>");
            position = consumed;
        } else {
            if let Some(character) = text[position..].chars().next() {
                output.push(character);
                position += character.len_utf8();
            } else {
                break;
            }
        }
    }

    output
}

struct ParsedUri {
    is_file: bool,
    path_start: Option<usize>,
    path_end: usize,
}

fn parse_uri(text: &str, start: usize) -> Option<ParsedUri> {
    let bytes = text.as_bytes();
    if !bytes.get(start)?.is_ascii_alphabetic() || !has_uri_token_boundary(text, start) {
        return None;
    }

    let mut scheme_end = start + 1;
    while bytes
        .get(scheme_end)
        .is_some_and(|byte| is_uri_scheme_byte(*byte))
    {
        scheme_end += 1;
    }
    if bytes.get(scheme_end) != Some(&b':')
        || bytes.get(scheme_end + 1..scheme_end + 3) != Some(b"//")
    {
        return None;
    }

    let authority_start = scheme_end + 3;
    let profile_span = bytes[start..scheme_end]
        .eq_ignore_ascii_case(b"file")
        .then(|| file_uri_profile_span(text, start, scheme_end))
        .flatten();
    let end = uri_token_end(text, start, profile_span);
    let path_start = bytes[authority_start..end]
        .iter()
        .position(|byte| *byte == b'/')
        .map(|relative| authority_start + relative);
    let path_end = path_start.map_or(end, |path_start| {
        bytes[path_start..end]
            .iter()
            .position(|byte| matches!(*byte, b'?' | b'#'))
            .map_or(end, |relative| path_start + relative)
    });

    Some(ParsedUri {
        is_file: bytes[start..scheme_end].eq_ignore_ascii_case(b"file"),
        path_start,
        path_end,
    })
}

fn has_uri_token_boundary(text: &str, start: usize) -> bool {
    text[..start].chars().next_back().is_none_or(|previous| {
        !previous.is_alphanumeric()
            && previous != '_'
            && !(previous.is_ascii() && is_uri_scheme_byte(previous as u8))
    })
}

fn uri_token_end(text: &str, start: usize, profile_span: Option<(usize, usize)>) -> usize {
    let quote_context = text[..start]
        .chars()
        .next_back()
        .filter(|character| matches!(character, '\'' | '"' | '`'));
    let has_enclosing_parenthesis = text[..start].ends_with('(');
    let mut nested_parentheses = 0usize;

    for (relative, character) in text[start..].char_indices() {
        let position = start + relative;
        if profile_span.is_some_and(|(profile_start, profile_end)| {
            (profile_start..profile_end).contains(&position)
        }) {
            continue;
        }
        if character.is_whitespace() || matches!(character, '<' | '>' | ']' | '}') {
            return position;
        }

        if let Some(quote) = quote_context {
            if character == quote && quote_closes_context(text, position + 1) {
                return position;
            }
        } else if matches!(character, '"' | '`') {
            return position;
        }

        if has_enclosing_parenthesis {
            if character == '(' {
                nested_parentheses += 1;
            } else if character == ')' {
                if nested_parentheses > 0 {
                    nested_parentheses -= 1;
                } else if closes_parenthesized_context(text, position + 1) {
                    return position;
                }
            }
        }
    }
    text.len()
}

fn quote_closes_context(text: &str, after_quote: usize) -> bool {
    let Some(next) = text[after_quote..].chars().next() else {
        return true;
    };
    if next.is_whitespace() {
        return true;
    }
    if matches!(next, ',' | ';') {
        let after_separator = after_quote + next.len_utf8();
        return text[after_separator..]
            .chars()
            .next()
            .is_none_or(|following| {
                following.is_whitespace() || matches!(following, '\'' | '"' | '`' | '[' | '(' | '{')
            })
            || starts_home_token_at(text, after_separator);
    }
    if matches!(next, ')' | ']' | '}' | '>') {
        return closes_after_delimiter(text, after_quote + next.len_utf8());
    }
    if matches!(next, '.' | ':' | '?' | '!') {
        return closes_after_delimiter(text, after_quote + next.len_utf8());
    }
    false
}

fn closes_parenthesized_context(text: &str, after_close: usize) -> bool {
    let Some(next) = text[after_close..].chars().next() else {
        return true;
    };
    if next.is_whitespace() || matches!(next, ')' | ']' | '}' | '>' | '"' | '\'' | '`') {
        return true;
    }
    if matches!(next, ',' | ';') {
        let after_separator = after_close + next.len_utf8();
        return text[after_separator..]
            .chars()
            .next()
            .is_none_or(|following| {
                following.is_whitespace() || matches!(following, '\'' | '"' | '`' | '[' | '(' | '{')
            })
            || starts_home_token_at(text, after_separator);
    }
    false
}

fn closes_after_delimiter(text: &str, after_delimiter: usize) -> bool {
    let Some(next) = text[after_delimiter..].chars().next() else {
        return true;
    };
    if next.is_whitespace() || matches!(next, ')' | ']' | '}' | '>') {
        return true;
    }
    if matches!(next, ',' | ';') {
        let after_separator = after_delimiter + next.len_utf8();
        return text[after_separator..]
            .chars()
            .next()
            .is_none_or(|following| {
                following.is_whitespace() || matches!(following, '\'' | '"' | '`' | '[' | '(' | '{')
            })
            || starts_home_token_at(text, after_separator);
    }
    false
}

fn starts_home_token_at(text: &str, start: usize) -> bool {
    if text
        .as_bytes()
        .get(start)
        .is_some_and(|_| home_path_end(text.as_bytes(), start).is_some())
    {
        return true;
    }

    let bytes = text.as_bytes();
    if !starts_with_ascii_case_insensitive(&bytes[start..], b"file://") {
        return false;
    }
    let scheme_end = start + 4;
    let Some(path_start) = uri_path_start(text, start, scheme_end + 3) else {
        return false;
    };
    starts_with_ascii_case_insensitive(&bytes[path_start..], b"/users/")
        || starts_with_ascii_case_insensitive(&bytes[path_start..], b"/home/")
        || (bytes
            .get(path_start + 1)
            .is_some_and(u8::is_ascii_alphabetic)
            && bytes.get(path_start + 2) == Some(&b':')
            && bytes
                .get(path_start + 3)
                .is_some_and(|byte| is_separator(*byte))
            && starts_with_ascii_case_insensitive(&bytes[path_start + 4..], b"users/"))
}

/// Find only the current URI's home-profile component, stopping at its
/// descendant separator or an enclosing context boundary. The main scanner
/// visits the URI token once afterward; this never scans the document suffix.
fn file_uri_profile_span(text: &str, start: usize, scheme_end: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let authority_start = scheme_end + 3;
    let path_start = uri_path_start(text, start, authority_start)?;
    let username_start = if starts_with_ascii_case_insensitive(&bytes[path_start..], b"/users/") {
        path_start + 7
    } else if starts_with_ascii_case_insensitive(&bytes[path_start..], b"/home/") {
        path_start + 6
    } else if bytes.get(path_start) == Some(&b'/')
        && bytes
            .get(path_start + 1)
            .is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(path_start + 2) == Some(&b':')
        && bytes
            .get(path_start + 3)
            .is_some_and(|byte| is_separator(*byte))
        && starts_with_ascii_case_insensitive(&bytes[path_start + 4..], b"users/")
    {
        path_start + 10
    } else {
        return None;
    };

    let quote_context = text[..start]
        .chars()
        .next_back()
        .filter(|character| matches!(character, '\'' | '"' | '`'));
    let has_enclosing_parenthesis = text[..start].ends_with('(');
    let mut nested_parentheses = 0usize;
    for (relative, character) in text[username_start..].char_indices() {
        let position = username_start + relative;
        if character == '/' {
            return (position > username_start).then_some((username_start, position));
        }
        if character.is_whitespace() || matches!(character, '?' | '#' | '<' | '>') {
            return None;
        }
        if quote_context
            .is_some_and(|quote| character == quote && quote_closes_context(text, position + 1))
        {
            return None;
        }
        if has_enclosing_parenthesis {
            if character == '(' {
                nested_parentheses += 1;
            } else if character == ')' {
                if nested_parentheses > 0 {
                    nested_parentheses -= 1;
                } else if closes_parenthesized_context(text, position + 1) {
                    return None;
                }
            }
        }
    }
    None
}

fn uri_path_start(text: &str, uri_start: usize, authority_start: usize) -> Option<usize> {
    let quote_context = text[..uri_start]
        .chars()
        .next_back()
        .filter(|character| matches!(character, '\'' | '"' | '`'));
    let has_enclosing_parenthesis = text[..uri_start].ends_with('(');
    for (relative, character) in text[authority_start..].char_indices() {
        let position = authority_start + relative;
        if character == '/' {
            return Some(position);
        }
        if character.is_whitespace() || matches!(character, '?' | '#' | '<' | '>' | ']' | '}') {
            return None;
        }
        if quote_context
            .is_some_and(|quote| character == quote && quote_closes_context(text, position + 1))
        {
            return None;
        }
        if quote_context.is_none() && matches!(character, '"' | '`') {
            return None;
        }
        if has_enclosing_parenthesis
            && character == ')'
            && closes_parenthesized_context(text, position + 1)
        {
            return None;
        }
    }
    None
}

fn is_uri_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
}

fn file_uri_home_path_end(bytes: &[u8], path_start: usize) -> Option<(usize, usize)> {
    if starts_with_ascii_case_insensitive(&bytes[path_start..], b"/users/")
        || starts_with_ascii_case_insensitive(&bytes[path_start..], b"/home/")
    {
        return home_path_end(bytes, path_start).map(|end| (path_start, end));
    }

    if bytes.get(path_start) == Some(&b'/')
        && bytes
            .get(path_start + 1)
            .is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(path_start + 2) == Some(&b':')
        && bytes
            .get(path_start + 3)
            .is_some_and(|byte| is_separator(*byte))
    {
        let drive_start = path_start + 1;
        return home_path_end(bytes, drive_start).map(|end| (drive_start, end));
    }

    None
}

fn has_path_boundary(text: &str, position: usize) -> bool {
    text[..position].chars().next_back().is_none_or(|previous| {
        !previous.is_alphanumeric() && previous != '_' && previous != '/' && previous != '\\'
    })
}

fn home_path_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes[start] == b'~' {
        return tilde_home_end(bytes, start);
    }

    if starts_with_ascii_case_insensitive(&bytes[start..], b"/home/") {
        return username_end(bytes, start + 6);
    }
    if starts_with_ascii_case_insensitive(&bytes[start..], b"/users/") {
        return username_end(bytes, start + 7);
    }

    if bytes.len() >= start + 9
        && bytes[start].is_ascii_alphabetic()
        && bytes[start + 1] == b':'
        && is_separator(bytes[start + 2])
        && starts_with_ascii_case_insensitive(&bytes[start + 3..start + 8], b"users")
        && is_separator(bytes[start + 8])
    {
        let username_start = start + 9;
        return quoted_profile_end(bytes, start, username_start)
            .or_else(|| unquoted_profile_end(bytes, username_start))
            .or_else(|| username_end(bytes, username_start));
    }

    unc_home_end(bytes, start)
}

fn tilde_home_end(bytes: &[u8], start: usize) -> Option<usize> {
    let after_tilde = start + 1;
    if after_tilde == bytes.len() || is_separator(bytes[after_tilde]) {
        return Some(after_tilde);
    }
    let next = std::str::from_utf8(&bytes[after_tilde..])
        .ok()?
        .chars()
        .next()?;
    if is_username_start(next) {
        return username_end(bytes, after_tilde);
    }
    (next.is_whitespace() || is_closing_delimiter(next)).then_some(after_tilde)
}

fn unc_home_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start + 2 > bytes.len() || !is_separator(bytes[start]) || !is_separator(bytes[start + 1]) {
        return None;
    }
    let server_end = segment_end(bytes, start + 2)?;
    if server_end == start + 2 || server_end == bytes.len() {
        return None;
    }
    let root_start = server_end + 1;
    let root_end = segment_end(bytes, root_start)?;
    if root_end == root_start || root_end == bytes.len() {
        return None;
    }
    let root = &bytes[root_start..root_end];
    if !root.eq_ignore_ascii_case(b"users") && !root.eq_ignore_ascii_case(b"home") {
        return None;
    }
    let username_start = root_end + 1;
    quoted_profile_end(bytes, start, username_start)
        .or_else(|| unquoted_profile_end(bytes, username_start))
        .or_else(|| username_end(bytes, username_start))
}

fn segment_end(bytes: &[u8], start: usize) -> Option<usize> {
    let relative_end = bytes[start..].iter().position(|byte| is_separator(*byte))?;
    Some(start + relative_end)
}

/// Include spaces in an unquoted Windows profile when a descendant separator
/// completes the same path. If whitespace introduces another absolute path,
/// leave the ordinary username scanner to preserve that prose boundary.
fn unquoted_profile_end(bytes: &[u8], start: usize) -> Option<usize> {
    let text = std::str::from_utf8(&bytes[start..]).ok()?;
    let separator = text
        .char_indices()
        .find(|(_, character)| matches!(character, '/' | '\\'))?
        .0;
    let whitespace = text
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map(|(offset, _)| offset);
    if whitespace.is_some_and(|offset| offset < separator)
        && has_new_absolute_path_after_whitespace(&text[..=separator])
    {
        return None;
    }

    let mut end = start + separator;
    while end > start && matches!(bytes[end - 1], b'.' | b',' | b';') {
        end -= 1;
    }
    (end > start).then_some(end)
}

fn has_new_absolute_path_after_whitespace(component: &str) -> bool {
    let bytes = component.as_bytes();
    for (offset, character) in component.char_indices() {
        if !character.is_whitespace() {
            continue;
        }
        let next = offset + character.len_utf8();
        if next >= bytes.len() {
            continue;
        }
        if is_separator(bytes[next]) {
            return true;
        }
        if bytes[next].is_ascii_alphabetic()
            && bytes.get(next + 1) == Some(&b':')
            && bytes.get(next + 2).is_some_and(|byte| is_separator(*byte))
        {
            return true;
        }
    }
    false
}

fn username_end(bytes: &[u8], start: usize) -> Option<usize> {
    let text = std::str::from_utf8(&bytes[start..]).ok()?;
    if let Some((offset, character)) = text
        .char_indices()
        .find(|(_, character)| character.is_whitespace() || matches!(character, '/' | '\\'))
        && matches!(character, '/' | '\\')
    {
        let mut end = start + offset;
        while end > start && matches!(bytes[end - 1], b'.' | b',' | b';') {
            end -= 1;
        }
        return (end > start).then_some(end);
    }

    let mut end = start;
    for (offset, character) in text.char_indices() {
        if matches!(character, '/' | '\\') || character.is_whitespace() {
            break;
        }
        if is_bare_component_boundary(character, text, offset) {
            break;
        }
        end = start + offset + character.len_utf8();
    }

    // Terminal punctuation belongs to surrounding text; interior symbols
    // remain part of the profile component.
    while end > start && matches!(bytes[end - 1], b'.' | b',' | b';') {
        end -= 1;
    }
    (end > start).then_some(end)
}

fn is_bare_component_boundary(character: char, text: &str, offset: usize) -> bool {
    if matches!(character, ')' | ']' | '}' | '>' | ',' | ';') {
        return true;
    }
    if character == '.' {
        let after = offset + character.len_utf8();
        return text[after..].chars().next().is_none_or(|next| {
            next.is_whitespace() || matches!(next, '/' | '\\' | ',' | ';' | ')' | ']' | '}')
        });
    }
    if matches!(character, '!' | '?') {
        let after = offset + character.len_utf8();
        return text[after..]
            .chars()
            .next()
            .is_none_or(|next| next.is_whitespace() || matches!(next, ')' | ']' | '}'));
    }
    if matches!(character, '\'' | '"') {
        let after = offset + character.len_utf8();
        return text[after..].chars().next().is_none_or(|next| {
            next.is_whitespace() || matches!(next, ',' | ';' | ')' | ']' | '}')
        });
    }
    false
}

/// Use an enclosing quote or the next path separator to bound a Windows
/// profile. A quoted bare profile ends at the matching quote; a profile with
/// descendants ends at the first separator. Quoted names may contain spaces
/// and symbols without consuming following prose.
fn quoted_profile_end(bytes: &[u8], path_start: usize, username_start: usize) -> Option<usize> {
    let quote = *bytes.get(path_start.checked_sub(1)?)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }

    let suffix = std::str::from_utf8(&bytes[username_start..]).ok()?;
    for (offset, character) in suffix.char_indices() {
        if matches!(character, '/' | '\\') {
            return (offset > 0).then_some(username_start + offset);
        }
        if character as u32 == quote as u32 {
            let after = offset + character.len_utf8();
            if suffix[after..].chars().next().is_none_or(|next| {
                next.is_whitespace() || matches!(next, ',' | ';' | ')' | ']' | '}')
            }) {
                return (offset > 0).then_some(username_start + offset);
            }
        }
    }
    None
}

fn is_username_start(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_closing_delimiter(character: char) -> bool {
    matches!(character, ')' | ']' | '}' | '\'' | '"' | '`' | '>')
}

fn is_separator(byte: u8) -> bool {
    byte == b'/' || byte == b'\\'
}

fn starts_with_ascii_case_insensitive(value: &[u8], prefix: &[u8]) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;

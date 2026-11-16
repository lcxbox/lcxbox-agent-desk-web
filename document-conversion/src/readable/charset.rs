//! Charset detection and decoding for fetched text.
//!
//! Priority: byte order mark, then the `Content-Type` charset, then (HTML
//! only) a `<meta>` or XML declaration within the first 1024 bytes, then
//! UTF-8. Unknown labels are ignored; unmappable bytes become U+FFFD.

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252, X_USER_DEFINED};

const PRESCAN_BYTES: usize = 1024;

/// Decodes `source`; when `truncated` is set an incomplete trailing character is dropped.
pub(super) fn decode(
    source: &[u8],
    charset: Option<&str>,
    sniff_html: bool,
    truncated: bool,
) -> String {
    let (encoding, body) = match Encoding::for_bom(source) {
        Some((encoding, bom_length)) => (encoding, &source[bom_length..]),
        None => {
            let encoding = charset
                .and_then(label_encoding)
                .or_else(|| sniff_html.then(|| prescan(source)).flatten())
                .unwrap_or(UTF_8);
            (encoding, source)
        }
    };
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut output = String::with_capacity(
        decoder
            .max_utf8_buffer_length(body.len())
            .unwrap_or(body.len()),
    );
    let mut remaining = body;
    loop {
        let (result, read, _) = decoder.decode_to_string(remaining, &mut output, !truncated);
        remaining = &remaining[read..];
        match result {
            encoding_rs::CoderResult::InputEmpty => return output,
            encoding_rs::CoderResult::OutputFull => output.reserve(remaining.len().max(16) * 3),
        }
    }
}

fn label_encoding(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label(label.trim().trim_matches(['"', '\'']).as_bytes())
}

/// Applies the in-document declaration rules: UTF-16 cannot be declared from
/// inside the document, and `x-user-defined` means windows-1252.
fn declared_encoding(label: &[u8]) -> Option<&'static Encoding> {
    let encoding = Encoding::for_label(label)?;
    Some(if encoding == UTF_16BE || encoding == UTF_16LE {
        UTF_8
    } else if encoding == X_USER_DEFINED {
        WINDOWS_1252
    } else {
        encoding
    })
}

/// Looks for an XML declaration or `<meta>` charset in the first 1024 bytes.
fn prescan(source: &[u8]) -> Option<&'static Encoding> {
    let bytes = &source[..source.len().min(PRESCAN_BYTES)];
    if bytes.starts_with(b"<?xml") {
        let end = find(bytes, 0, b"?>").unwrap_or(bytes.len());
        let mut cursor = 5;
        while let Some((name, value)) = next_attribute(&bytes[..end], &mut cursor) {
            if name == b"encoding" {
                return declared_encoding(&value);
            }
        }
    }
    let mut cursor = 0;
    while cursor < bytes.len() {
        let rest = &bytes[cursor..];
        if rest.starts_with(b"<!--") {
            cursor = find(bytes, cursor + 2, b"-->")? + 3;
        } else if starts_with_ignore_case(rest, b"<meta")
            && rest
                .get(5)
                .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'/')
        {
            cursor += 5;
            if let Some(encoding) = meta_encoding(bytes, &mut cursor) {
                return Some(encoding);
            }
        } else if rest.first() == Some(&b'<')
            && (rest.get(1).is_some_and(u8::is_ascii_alphabetic)
                || (rest.get(1) == Some(&b'/') && rest.get(2).is_some_and(u8::is_ascii_alphabetic)))
        {
            cursor += rest
                .iter()
                .position(|byte| byte.is_ascii_whitespace() || *byte == b'>')
                .unwrap_or(rest.len());
            while next_attribute(bytes, &mut cursor).is_some() {}
        } else if rest.starts_with(b"<!") || rest.starts_with(b"</") || rest.starts_with(b"<?") {
            cursor = find(bytes, cursor, b">")? + 1;
        } else {
            cursor += 1;
        }
    }
    None
}

fn meta_encoding(bytes: &[u8], cursor: &mut usize) -> Option<&'static Encoding> {
    let mut http_equiv_content_type = false;
    let mut content_charset = None;
    let mut charset = None;
    while let Some((name, value)) = next_attribute(bytes, cursor) {
        match name.as_slice() {
            b"http-equiv" => http_equiv_content_type = value.eq_ignore_ascii_case(b"content-type"),
            b"content" if content_charset.is_none() => {
                content_charset = charset_from_content(&value)
            }
            b"charset" if charset.is_none() => charset = Some(value),
            _ => {}
        }
    }
    match (charset, content_charset) {
        (Some(label), _) => declared_encoding(&label),
        (None, Some(label)) if http_equiv_content_type => declared_encoding(&label),
        _ => None,
    }
}

/// Extracts the charset from a `content="text/html; charset=..."` value.
fn charset_from_content(content: &[u8]) -> Option<Vec<u8>> {
    let mut search_from = 0;
    loop {
        let start = find_ignore_case(content, search_from, b"charset")? + 7;
        let mut index = start;
        while content.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if content.get(index) != Some(&b'=') {
            search_from = start;
            continue;
        }
        index += 1;
        while content.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        let value = &content[index..];
        return match value.first() {
            Some(quote @ (b'"' | b'\'')) => {
                let end = value[1..].iter().position(|byte| byte == quote)?;
                Some(value[1..1 + end].to_vec())
            }
            Some(_) => {
                let end = value
                    .iter()
                    .position(|byte| byte.is_ascii_whitespace() || *byte == b';')
                    .unwrap_or(value.len());
                Some(value[..end].to_vec())
            }
            None => None,
        };
    }
}

/// Reads the next tag attribute following the WHATWG prescan rules; returns
/// `None` at `>` or the end of input. Names are lowercased.
fn next_attribute(bytes: &[u8], cursor: &mut usize) -> Option<(Vec<u8>, Vec<u8>)> {
    let at = |index: usize| bytes.get(index).copied();
    while at(*cursor).is_some_and(|byte| byte.is_ascii_whitespace() || byte == b'/') {
        *cursor += 1;
    }
    match at(*cursor) {
        None | Some(b'>') => return None,
        _ => {}
    }
    let mut name = Vec::new();
    while let Some(byte) = at(*cursor) {
        if (byte == b'=' && !name.is_empty())
            || byte.is_ascii_whitespace()
            || byte == b'/'
            || byte == b'>'
        {
            break;
        }
        name.push(byte.to_ascii_lowercase());
        *cursor += 1;
    }
    while at(*cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
        *cursor += 1;
    }
    if at(*cursor) != Some(b'=') {
        return Some((name, Vec::new()));
    }
    *cursor += 1;
    while at(*cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
        *cursor += 1;
    }
    let mut value = Vec::new();
    match at(*cursor) {
        Some(quote @ (b'"' | b'\'')) => {
            *cursor += 1;
            while let Some(byte) = at(*cursor) {
                *cursor += 1;
                if byte == quote {
                    break;
                }
                value.push(byte);
            }
        }
        _ => {
            while let Some(byte) = at(*cursor) {
                if byte.is_ascii_whitespace() || byte == b'>' {
                    break;
                }
                value.push(byte);
                *cursor += 1;
            }
        }
    }
    Some((name, value))
}

fn starts_with_ignore_case(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

fn find_ignore_case(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|position| from + position)
}

#[cfg(test)]
mod tests;

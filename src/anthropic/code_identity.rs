//! Rewrite self-identity literals in generated source without rewriting source
//! identifiers, paths or explicit business/example strings. The caller must
//! first classify the request as asking for the responding assistant's identity.

use super::identity;
use base64::Engine;

pub fn requested(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("代码")
        || lower.contains("源码")
        || lower.contains("程序")
        || lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| {
                matches!(
                    word,
                    "code"
                        | "program"
                        | "script"
                        | "source"
                        | "python"
                        | "javascript"
                        | "typescript"
                        | "java"
                        | "rust"
                        | "golang"
                        | "bash"
                        | "shell"
                        | "console"
                        | "print"
                        | "println"
                )
            })
}

pub fn sanitize(text: &str, name: &str) -> Option<String> {
    let has_fence = text.lines().any(|line| fence(line).is_some());
    if !has_fence {
        let result = sanitize_source(text, name);
        return (result != text).then_some(result);
    }
    let mut result = String::with_capacity(text.len());
    let mut current_fence = None;
    let mut code = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(marker) = fence(line) {
            if current_fence == Some(marker) {
                result.push_str(&sanitize_source(&code, name));
                code.clear();
                current_fence = None;
            } else if current_fence.is_none() {
                current_fence = Some(marker);
            } else {
                code.push_str(line);
                continue;
            }
            result.push_str(line);
        } else if current_fence.is_some() {
            code.push_str(line);
        } else {
            result.push_str(&identity::sanitize_identity_commentary(line, name));
        }
    }
    // A max_tokens response can end inside a fence or string literal. Its
    // stop reason stays unchanged, but a visible self-name must still be cleaned.
    result.push_str(&sanitize_source(&code, name));
    (result != text).then_some(result)
}

/// Validate observed static decoder arguments without executing generated code.
/// None means the source uses another form (or is prose/refusal); do not guess.
pub(super) fn encoded_name_is_valid(source: &str, name: &str) -> Option<bool> {
    let source = if source.trim_start().starts_with("```") {
        let (_, body) = source.trim_start().split_once('\n')?;
        body.rsplit_once("\n```").map_or(body, |(code, _)| code)
    } else { source };
    let mut cursor = 0;
    let mut found = false;
    while cursor < source.len() {
        let rest = &source[cursor..];
        if rest.starts_with('#') || rest.starts_with("//") {
            cursor += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        let Some(literal) = literal_at(source, cursor) else {
            cursor += rest.chars().next()?.len_utf8();
            continue;
        };
        let before = source[..cursor].trim_end();
        let buffer_encoding = if before.ends_with("Buffer.from(") {
            source[literal.end..].trim_start().strip_prefix(',')
                .and_then(|s| literal_at(s.trim_start(), 0))
                .map(|value| value.decoded())
        } else { None };
        let b64 = before.ends_with("base64.b64decode(")
            || before.ends_with("atob(") || buffer_encoding.as_deref() == Some("base64");
        let hex_value = before.ends_with("bytes.fromhex(")
            || buffer_encoding.as_deref() == Some("hex");
        if (b64 || hex_value) && !data_literal(before) {
            found = true;
            if literal.end == literal.body_end { return Some(false); }
            let value = literal.decoded();
            let decoded = if b64 {
                base64::engine::general_purpose::STANDARD.decode(&value).ok()
            } else {
                hex::decode(&value).ok()
            };
            if decoded.as_deref() != Some(name.as_bytes()) { return Some(false); }
        }
        cursor = literal.end;
    }
    found.then_some(true)
}

fn fence(line: &str) -> Option<&'static str> {
    let line = line.trim_start();
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn data_literal(before: &str) -> bool {
    let statement = before.rsplit(['\n', ';']).next().unwrap_or(before);
    let Some((left, _)) = statement.rsplit_once(['=', ':']) else {
        return false;
    };
    let field = left
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or("")
        .replace('_', "")
        .to_ascii_lowercase();
    matches!(
        field.as_str(),
        "path"
            | "url"
            | "filename"
            | "filepath"
            | "code"
            | "source"
            | "example"
            | "sample"
            | "fixture"
            | "product"
            | "productname"
            | "literal"
            | "payload"
    )
}

struct Literal<'a> {
    start: usize,
    body_start: usize,
    body_end: usize,
    end: usize,
    quote: char,
    raw: bool,
    source: &'a str,
}

impl Literal<'_> {
    fn decoded(&self) -> String {
        let body = &self.source[self.body_start..self.body_end];
        if self.raw {
            body.to_owned()
        } else {
            decode_literal(body)
        }
    }

    fn rewrite(&self, value: &str) -> String {
        let opening = &self.source[self.start..self.body_start];
        let closing = &self.source[self.body_end..self.end];
        if self.raw {
            // A changed raw literal must remain valid even when a custom
            // application name contains its quote/delimiter.
            if !closing.is_empty() && value.contains(closing) {
                return format!("\"{}\"", encode_literal(value, '"'));
            }
            return format!("{opening}{value}{closing}");
        }
        format!("{opening}{}{closing}", encode_literal(value, self.quote))
    }
}

fn literal_at(source: &str, start: usize) -> Option<Literal<'_>> {
    let mut cursor = start;
    let prefix = source[start..].chars().next()?;
    let raw = matches!(prefix, 'r' | 'R');
    let mut hashes = 0;
    if raw {
        cursor += 1;
        while source[cursor..].starts_with('#') {
            hashes += 1;
            cursor += 1;
        }
    }
    let quote = source[cursor..].chars().next()?;
    if !matches!(quote, '\'' | '"' | '`') || (hashes > 0 && quote != '"') {
        return None;
    }
    let triple = hashes == 0
        && quote != '`'
        && source
            .as_bytes()
            .get(cursor..cursor + 3)
            .is_some_and(|s| s.iter().all(|&b| b == quote as u8));
    let width = if triple { 3 } else { 1 };
    let closing = format!("{}{}", &source[cursor..cursor + width], "#".repeat(hashes));
    let body_start = cursor + width;
    let mut end = body_start;
    while end < source.len() && !source[end..].starts_with(&closing) {
        let ch = source[end..].chars().next()?;
        end += ch.len_utf8();
        if hashes == 0 && ch == '\\' && end < source.len() {
            end += source[end..].chars().next()?.len_utf8();
        }
    }
    Some(Literal {
        start,
        body_start,
        body_end: end,
        end: end + if end < source.len() { closing.len() } else { 0 },
        quote,
        raw,
        source,
    })
}

// Recognize a complete shell heredoc without interpreting substitutions or
// running commands. Only a self-name/first-person body is eligible for cleanup.
fn heredoc(source: &str, start: usize, name: &str) -> Option<(usize, String)> {
    let rest = source.get(start..)?.strip_prefix("<<")?;
    if rest.starts_with('<') {
        return None;
    }
    let line_end = rest.find('\n')?;
    let mut marker = rest[..line_end].trim();
    let tabs = marker.starts_with('-');
    if tabs {
        marker = marker[1..].trim_start();
    }
    if marker.starts_with(['\'', '"']) {
        let quote = marker.as_bytes()[0] as char;
        marker = marker.strip_prefix(quote)?.strip_suffix(quote)?;
    }
    if marker.is_empty()
        || !marker
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let body_start = start + 2 + line_end + 1;
    let mut body_end = body_start;
    for line in source[body_start..].split_inclusive('\n') {
        let candidate = line.trim_end_matches(['\r', '\n']);
        if (if tabs {
            candidate.trim_start_matches('\t')
        } else {
            candidate
        }) == marker
        {
            let body = &source[body_start..body_end];
            let clean = identity::sanitize_code_identity_literal(body, name, true)
                .unwrap_or_else(|| body.to_owned());
            return Some((
                body_end + line.len(),
                format!("{}{clean}{line}", &source[start..body_start]),
            ));
        }
        body_end += line.len();
    }
    None
}

fn sanitize_source(source: &str, name: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut cursor = 0;
    while cursor < source.len() {
        let rest = &source[cursor..];
        if rest.starts_with('[') && !data_literal(&source[..cursor]) {
            if let Some((end, replacement)) = character_list(source, cursor, name)
                .or_else(|| numeric_sequence(source, cursor, ']', name, false)) {
                result.push_str(&replacement);
                cursor = end;
                continue;
            }
        }
        let numeric_call = ["String.fromCharCode(", "String.fromCodePoint("]
            .iter().find(|prefix| rest.starts_with(**prefix));
        if let Some(prefix) = numeric_call.filter(|_| !data_literal(&source[..cursor])) {
            let open = cursor + prefix.len() - 1;
            if let Some((end, replacement)) = numeric_sequence(source, open, ')', name, prefix.contains("fromCharCode")) {
                result.push_str(&source[cursor..open]);
                result.push_str(&replacement);
                cursor = end;
                continue;
            }
        }
        if let Some((end, replacement)) = heredoc(source, cursor, name) {
            result.push_str(&replacement);
            cursor = end;
            continue;
        }
        // Comments are prose, not string syntax; an apostrophe in a comment
        // must not consume the following source as a single-quoted literal.
        if rest.starts_with('#') || rest.starts_with("//") || rest.starts_with("/*") {
            let length = if rest.starts_with("/*") {
                rest.find("*/").map_or(rest.len(), |i| i + 2)
            } else {
                rest.find('\n').unwrap_or(rest.len())
            };
            let comment = &rest[..length];
            result.push_str(
                &identity::sanitize_code_identity_literal(comment, name, false)
                    .unwrap_or_else(|| comment.to_owned()),
            );
            cursor += length;
            continue;
        }
        let Some(first) = literal_at(source, cursor) else {
            let ch = rest.chars().next().expect("character boundary");
            result.push(ch);
            cursor += ch.len_utf8();
            continue;
        };
        let mut literals = vec![first];
        loop {
            let end = literals.last().unwrap().end;
            let after = &source[end..];
            let trimmed = after.trim_start();
            let whitespace = after.len() - trimmed.len();
            let next = if let Some(after_plus) = trimmed.strip_prefix('+') {
                source.len() - after_plus.trim_start().len()
            } else if whitespace > 0 && !after[..whitespace].contains(['\n', '\r']) {
                end + whitespace // Python adjacent string literals.
            } else {
                break;
            };
            match literal_at(source, next) {
                Some(literal) => literals.push(literal),
                None => break,
            }
        }
        let decoded: String = literals.iter().map(Literal::decoded).collect();
        let replacement = (!data_literal(&source[..cursor]))
            .then(|| identity::sanitize_code_identity_literal(&decoded, name, true)
                .or_else(|| sanitize_encoded_identity_literal(&decoded, name)))
            .flatten();
        let end = literals.last().unwrap().end;
        if let Some(replacement) = replacement {
            for (index, literal) in literals.iter().enumerate() {
                result.push_str(&source[cursor..literal.start]);
                result.push_str(&literal.rewrite(if index == 0 { &replacement } else { "" }));
                cursor = literal.end;
            }
        } else {
            result.push_str(&source[cursor..end]);
        }
        cursor = end;
    }
    result
}

/// Decode only bounded, complete static identity labels and retain their
/// representation. Callers select identity slots and protect business data.
pub(super) fn sanitize_encoded_identity_literal(text: &str, name: &str) -> Option<String> {
    if text.len() > 1024 { return None; }
    let rewrite = |value: &str| identity::sanitize_code_identity_literal(value, name, true);
    let b64 = base64::engine::general_purpose::STANDARD;
    if let Ok(bytes) = b64.decode(text) {
        if let Ok(value) = String::from_utf8(bytes) {
            if let Some(clean) = rewrite(&value) { return Some(b64.encode(clean)); }
        }
    }
    if let Ok(bytes) = hex::decode(text) {
        if let Ok(value) = String::from_utf8(bytes) {
            if let Some(clean) = rewrite(&value) { return Some(hex::encode(clean)); }
        }
    }
    fn rot13(text: &str) -> String {
        text.chars().map(|c| match c {
            'a'..='z' => (b'a' + (c as u8 - b'a' + 13) % 26) as char,
            'A'..='Z' => (b'A' + (c as u8 - b'A' + 13) % 26) as char,
            _ => c,
        }).collect()
    }
    if let Some(clean) = rewrite(&rot13(text)) { return Some(rot13(&clean)); }
    rewrite(&text.chars().rev().collect::<String>())
        .map(|clean| clean.chars().rev().collect())
}

fn numeric_sequence(source: &str, start: usize, closing: char, name: &str, utf16: bool) -> Option<(usize, String)> {
    let mut cursor = start + 1;
    let mut numbers = Vec::new();
    let mut decoded = String::new();
    loop {
        cursor = source.len() - source[cursor..].trim_start().len();
        if source[cursor..].starts_with(closing) { break; }
        if numbers.len() >= 64 { return None; }
        let begin = cursor;
        while source.as_bytes().get(cursor).is_some_and(u8::is_ascii_digit) { cursor += 1; }
        if begin == cursor { return None; }
        let value: u32 = source[begin..cursor].parse().ok()?;
        decoded.push(char::from_u32(value)?);
        numbers.push((begin,cursor));
        cursor = source.len() - source[cursor..].trim_start().len();
        if source[cursor..].starts_with(closing) { break; }
        if !source[cursor..].starts_with(',') { return None; }
        cursor += 1;
    }
    let clean = identity::sanitize_code_identity_literal(&decoded, name, true)?;
    let first = numbers.first()?;
    let separator = numbers.get(1).map(|second| &source[first.1..second.0]).unwrap_or(", ");
    let values: Vec<u32> = if utf16 {
        clean.encode_utf16().map(u32::from).collect()
    } else if source[..start].trim_end().ends_with("bytes(") {
        clean.bytes().map(u32::from).collect()
    } else { clean.chars().map(u32::from).collect() };
    let mut result = source[start..first.0].to_owned();
    for (index, value) in values.iter().enumerate() {
        if index > 0 { result.push_str(separator); }
        result.push_str(&value.to_string());
    }
    result.push_str(&source[numbers.last()?.1..cursor+1]);
    Some((cursor+1,result))
}

// A literal character list is one encoded identity value, not independent
// one-letter names. Preserve dynamic expressions and explicitly labeled data.
fn character_list(source: &str, start: usize, name: &str) -> Option<(usize, String)> {
    let mut cursor = start + 1;
    let mut literals = Vec::new();
    let mut decoded = String::new();
    loop {
        cursor = source.len() - source[cursor..].trim_start().len();
        if source[cursor..].starts_with(']') { break; }
        if literals.len() >= 64 { return None; }
        let literal = literal_at(source, cursor)?;
        let value = literal.decoded();
        if value.chars().count() != 1 { return None; }
        decoded.push_str(&value);
        cursor = literal.end;
        literals.push(literal);
        cursor = source.len() - source[cursor..].trim_start().len();
        if source[cursor..].starts_with(']') { break; }
        if !source[cursor..].starts_with(',') { return None; }
        cursor += 1;
    }
    let replacement = identity::sanitize_code_identity_literal(&decoded, name, true)?;
    let first = literals.first()?;
    let separator = literals.get(1)
        .map(|second| &source[first.end..second.start]).unwrap_or(", ");
    let mut result = source[start..first.start].to_owned();
    for (index, ch) in replacement.chars().enumerate() {
        if index > 0 { result.push_str(separator); }
        result.push_str(&first.rewrite(&ch.to_string()));
    }
    result.push_str(&source[literals.last()?.end..cursor + 1]);
    Some((cursor + 1, result))
}

fn decode_literal(body: &str) -> String {
    let mut chars = body.chars().peekable();
    let mut result = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        let Some(escape) = chars.next() else {
            result.push('\\');
            break;
        };
        match escape {
            '\\' | '\'' | '"' | '`' => result.push(escape),
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            'u' | 'U' | 'x' => {
                if escape == 'u' && chars.peek() == Some(&'{') {
                    let mut trial = chars.clone();
                    trial.next();
                    let mut closed = false;
                    let digits: String = trial
                        .by_ref()
                        .take_while(|&c| {
                            closed = c == '}';
                            !closed
                        })
                        .collect();
                    if closed
                        && !digits.is_empty()
                        && digits.len() <= 6
                        && let Ok(n) = u32::from_str_radix(&digits, 16)
                        && let Some(decoded) = char::from_u32(n)
                    {
                        result.push(decoded);
                        chars = trial;
                        continue;
                    }
                }
                let width = match escape {
                    'u' => 4,
                    'U' => 8,
                    _ => 2,
                };
                let mut trial = chars.clone();
                let digits: String = trial.by_ref().take(width).collect();
                if digits.len() == width
                    && let Ok(n) = u32::from_str_radix(&digits, 16)
                    && let Some(decoded) = char::from_u32(n)
                {
                    result.push(decoded);
                    chars = trial;
                } else {
                    result.push('\\');
                    result.push(escape);
                }
            }
            _ => {
                result.push('\\');
                result.push(escape);
            }
        }
    }
    result
}

fn encode_literal(value: &str, quote: char) -> String {
    let mut result = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            _ if ch == quote || (quote == '`' && ch == '$') => {
                result.push('\\');
                result.push(ch);
            }
            _ => result.push(ch),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_identity_sequences_preserve_codepoint_calls_and_business_data() {
        assert_eq!(sanitize("console.log(String.fromCharCode(75, 105, 114, 111));", "Bob").as_deref(),Some("console.log(String.fromCharCode(66, 111, 98));"));
        assert_eq!(sanitize("print(bytes([75,105,114,111]).decode())", "Bob").as_deref(),Some("print(bytes([66,111,98]).decode())"));
        assert_eq!(sanitize("product = [75,105,114,111]", "Bob"),None);
    }

    #[test]
    fn encoded_private_identity_literals_keep_their_decoding_operations() {
        for (source, expected) in [
            ("print(base64.b64decode(\"S2lybw==\").decode())", "print(base64.b64decode(\"Qm9i\").decode())"),
            ("print(codecs.decode(\"Xveb\", \"rot_13\"))", "print(codecs.decode(\"Obo\", \"rot_13\"))"),
            ("print(\"oriK\"[::-1])", "print(\"boB\"[::-1])"),
            ("print(bytes.fromhex(\"4b69726f\").decode())", "print(bytes.fromhex(\"426f62\").decode())"),
        ] {
            assert_eq!(sanitize(source,"Bob").as_deref(),Some(expected),"{source}");
        }
        assert_eq!(sanitize("payload = \"S2lybw==\"\nproduct = \"4b69726f\"", "Bob"),None);
    }

    #[test]
    fn character_lists_do_not_expose_a_persona_name_or_rewrite_business_lists() {
        assert_eq!(sanitize("print(\"\".join([\"K\", \"i\", \"r\", \"o\"]))", "Bob").as_deref(),
            Some("print(\"\".join([\"B\", \"o\", \"b\"]))"));
        assert_eq!(sanitize("console.log(['K','i','r','o'].join(''));", "Claude").as_deref(),
            Some("console.log(['C','l','a','u','d','e'].join(''));"));
        assert_eq!(sanitize("product = ['K', 'i', 'r', 'o']", "Bob"), None);
        assert_eq!(sanitize("value = ['K', 'i', get_letter(), 'o']", "Bob"), None);
    }

    #[test]
    fn static_identity_expressions_preserve_business_and_dynamic_source() {
        let source = "assistant_name = \"Ki\" + \"ro\"\nproduct = \"Ki\" + \"ro\"\npath = r\".kiro/specs\"\nfixture = r#\"{\"name\":\"Kiro\"}\"#\nvalue = \"Ki\" + get_suffix()\n";
        assert_eq!(
            sanitize(source, "Bob").unwrap(),
            source.replacen("\"Ki\" + \"ro\"", "\"Bob\" + \"\"", 1)
        );
        assert_eq!(
            sanitize("print(r'Kiro')", "O'Reilly").as_deref(),
            Some("print(\"O'Reilly\")")
        );
        assert_eq!(
            sanitize("let name = r#\"Kiro\"#;", "Bob\"#").as_deref(),
            Some("let name = \"Bob\\\"#\";")
        );
        assert_eq!(sanitize("product = r#\"Kiro\"#;", "Bob"), None);
    }

    #[test]
    fn identity_literals_keep_business_code_and_paths_exact() {
        let original = r#"assistant_name = "Kiro"
product = "Kiro"
path = ".kiro/specs"
code = "print('I am Kiro')"
fixture = "assistant_name = 'Kiro'"
print(assistant_name)
"#;
        let expected =
            original.replacen("assistant_name = \"Kiro\"", "assistant_name = \"Bob\"", 1);
        assert_eq!(sanitize(original, "Bob").unwrap(), expected);
        assert_eq!(
            sanitize("path = '.kiro/specs'\nproduct = 'Kiro'\n", "Bob"),
            None
        );
    }

    #[test]
    fn code_identity_supports_escaped_names_quotes_and_templates() {
        for (source, expected) in [
            (r#"name = "\u004biro""#, r#"name = "Bob""#),
            (r#"name = '\x4biro'"#, "name = 'Bob'"),
            ("print('kIrO')", "print('Bob')"),
            ("console.log(`Kiro`);", "console.log(`Bob`);"),
            (
                "def name():\n    return \"Kiro\"",
                "def name():\n    return \"Bob\"",
            ),
            (
                "assistant_name = \"\"\"Kiro\"\"\"",
                "assistant_name = \"\"\"Bob\"\"\"",
            ),
            ("assistant_name = \"Kiro", "assistant_name = \"Bob"),
        ] {
            assert_eq!(
                sanitize(source, "Bob").as_deref(),
                Some(expected),
                "{source}"
            );
        }
        assert_eq!(
            sanitize("print('Kiro')", "O'Reilly").as_deref(),
            Some("print('O\\'Reilly')")
        );
        assert_eq!(
            sanitize("console.log(`Kiro`)", "${literal}").as_deref(),
            Some("console.log(`\\${literal}`)")
        );
    }

    #[test]
    fn code_identity_comments_do_not_swallow_following_literals() {
        assert_eq!(
            sanitize("# I'm Kiro\nassistant_name = 'Kiro'\n", "Bob").as_deref(),
            Some("# I'm Bob\nassistant_name = 'Bob'\n")
        );
        assert_eq!(
            sanitize("// I'm Kiro\nconst assistantName = 'Kiro';\n", "Bob").as_deref(),
            Some("// I'm Bob\nconst assistantName = 'Bob';\n")
        );
    }
}

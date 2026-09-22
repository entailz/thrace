/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Lightweight highlighting for fenced Markdown code. Unknown languages stay legible.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    String,
    Comment,
    Number,
}

pub fn tokenize<'a>(code: &'a str, language: &str) -> Vec<(&'a str, Kind)> {
    let lang = language
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut out = Vec::new();
    let mut offset = 0;
    for line in code.split_inclusive('\n') {
        let mut i = 0;
        while i < line.len() {
            let rest = &line[i..];
            let comment = match lang.as_str() {
                "python" | "py" | "bash" | "sh" | "shell" | "toml" | "yaml" | "yml" => {
                    rest.starts_with('#')
                }
                "sql" => rest.starts_with("--"),
                "html" | "xml" => rest.starts_with("<!--"),
                _ => rest.starts_with("//"),
            };
            if comment {
                out.push((&code[offset + i..offset + line.len()], Kind::Comment));
                break;
            }
            let first = rest.chars().next().unwrap();
            if matches!(first, '"' | '\'' | '`') {
                let quote = first;
                let mut end = first.len_utf8();
                let mut escaped = false;
                for ch in rest[end..].chars() {
                    end += ch.len_utf8();
                    if ch == quote && !escaped {
                        break;
                    }
                    if ch == '\\' && !escaped {
                        escaped = true;
                    } else {
                        escaped = false;
                    }
                }
                out.push((&code[offset + i..offset + i + end], Kind::String));
                i += end;
                continue;
            }
            if first.is_ascii_digit() {
                let end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_')
                    .unwrap_or(rest.len());
                out.push((&code[offset + i..offset + i + end], Kind::Number));
                i += end;
                continue;
            }
            if first.is_ascii_alphabetic() || first == '_' {
                let end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .unwrap_or(rest.len());
                let word = &code[offset + i..offset + i + end];
                let kind = if keyword(&lang, word) {
                    Kind::Keyword
                } else {
                    Kind::Plain
                };
                out.push((word, kind));
                i += end;
                continue;
            }
            let end = first.len_utf8();
            out.push((&code[offset + i..offset + i + end], Kind::Plain));
            i += end;
        }
        offset += line.len();
    }
    out
}

fn keyword(lang: &str, word: &str) -> bool {
    let words: &[&str] = match lang {
        "rust" | "rs" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
            "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
            "type", "unsafe", "use", "where", "while",
        ],
        "javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx" => &[
            "async",
            "await",
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "default",
            "else",
            "export",
            "false",
            "finally",
            "for",
            "function",
            "if",
            "import",
            "in",
            "interface",
            "let",
            "new",
            "null",
            "return",
            "switch",
            "throw",
            "true",
            "try",
            "type",
            "undefined",
            "var",
            "while",
        ],
        "python" | "py" => &[
            "and", "as", "async", "await", "break", "class", "continue", "def", "del", "elif",
            "else", "except", "False", "finally", "for", "from", "if", "import", "in", "is",
            "lambda", "None", "not", "or", "pass", "raise", "return", "True", "try", "while",
            "with", "yield",
        ],
        "json" | "jsonc" => &["true", "false", "null"],
        "sql" => &[
            "SELECT", "FROM", "WHERE", "JOIN", "ON", "INSERT", "UPDATE", "DELETE", "CREATE",
            "TABLE", "AS", "AND", "OR", "NULL", "select", "from", "where", "join", "on", "insert",
            "update", "delete", "create", "table", "as", "and", "or", "null",
        ],
        "bash" | "sh" | "shell" => &[
            "case", "do", "done", "echo", "else", "esac", "export", "fi", "for", "function", "if",
            "in", "local", "read", "return", "then", "while",
        ],
        _ => &[],
    };
    words.contains(&word)
}

pub fn layout(code: &str, language: &str, size: f32, base: egui::Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let font = egui::FontId::monospace(size);
    let dark_background = u16::from(base.r()) + u16::from(base.g()) + u16::from(base.b()) > 384;
    for (text, kind) in tokenize(code, language) {
        let color = match kind {
            Kind::Plain => base,
            Kind::Keyword if dark_background => egui::Color32::from_rgb(198, 155, 255),
            Kind::Keyword => egui::Color32::from_rgb(107, 55, 171),
            Kind::String if dark_background => egui::Color32::from_rgb(163, 214, 143),
            Kind::String => egui::Color32::from_rgb(35, 116, 54),
            Kind::Comment => base.gamma_multiply(0.58),
            Kind::Number if dark_background => egui::Color32::from_rgb(242, 190, 120),
            Kind::Number => egui::Color32::from_rgb(153, 86, 27),
        };
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color,
                ..Default::default()
            },
        );
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keywords_strings_and_comments_keep_source_order() {
        let code = "let answer = 42; // note\nprintln!(\"hi\");";
        let parts = tokenize(code, "rust");
        assert_eq!(parts.iter().map(|(s, _)| *s).collect::<String>(), code);
        assert!(parts.contains(&("let", Kind::Keyword)));
        assert!(parts.contains(&("42", Kind::Number)));
        assert!(parts.contains(&("\"hi\"", Kind::String)));
        assert!(parts
            .iter()
            .any(|(s, kind)| s.starts_with("//") && *kind == Kind::Comment));
    }
}

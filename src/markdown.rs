/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Matrix message bodies → egui spans.
//!
//! `formatted_body` HTML first, plain `body` via a small CommonMark subset as fallback. Flat
//! `Span`s rendered with `RichText`; no webview.

/// One styled run inside a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: Option<String>,
}

impl Span {
    pub fn plain(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            bold: false,
            italic: false,
            code: false,
            link: None,
        }
    }
}

/// A fenced/indented code block (rendered in its own frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub lang: String,
    pub code: String,
}

/// Rendered message; blocks stay in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RichMessage {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Spans(Vec<Span>),
    Code(CodeBlock),
}

impl RichMessage {
    pub fn plain(text: &str) -> Self {
        Self {
            blocks: vec![Block::Spans(vec![Span::plain(text)])],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Prefer `formatted_body` HTML; fall back to plain `body`.
pub fn render_message(body: &str, formatted_body: Option<&str>) -> RichMessage {
    let mut msg = None;
    if let Some(html) = formatted_body {
        let rendered = render_html(html);
        if !rendered.is_empty() {
            msg = Some(rendered);
        }
    }
    let mut msg = msg.unwrap_or_else(|| render_markdown_subset(body));
    for block in &mut msg.blocks {
        if let Block::Spans(spans) = block {
            // URLs first, so an mxid inside a URL is already a link when mentions run.
            *spans = linkify_mentions(linkify_spans(std::mem::take(spans)));
        }
    }
    msg
}

/// Bare `@user:server` text → mention link with the same `matrix.to` href pills use, so the
/// renderer has one case for both.
fn linkify_mentions(spans: Vec<Span>) -> Vec<Span> {
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        if span.link.is_some() || span.code {
            out.push(span);
            continue;
        }
        let text = span.text.clone();
        let mut rest = text.as_str();
        let mut split_any = false;
        while let Some((start, end)) = next_mention(rest) {
            let (before, from) = rest.split_at(start);
            let mxid = &from[..end];
            if !before.is_empty() {
                out.push(Span {
                    text: before.to_owned(),
                    ..span.clone()
                });
            }
            out.push(Span {
                text: mxid.to_owned(),
                link: Some(format!("https://matrix.to/#/{mxid}")),
                ..span.clone()
            });
            rest = &from[end..];
            split_any = true;
        }
        if !rest.is_empty() || !split_any {
            out.push(Span {
                text: rest.to_owned(),
                ..span.clone()
            });
        }
    }
    out
}

/// Byte range `(start, len_from_start)` of the first `@user:server` in `s`.
fn next_mention(s: &str) -> Option<(usize, usize)> {
    let mut search = 0;
    while let Some(rel) = s[search..].find('@') {
        let at = search + rel;
        // Must start a word; `mail@host:1` is not a mention.
        let boundary = s[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if boundary {
            if let Some(len) = mention_len(&s[at..]) {
                return Some((at, len));
            }
        }
        search = at + 1;
    }
    None
}

/// Length of the `@localpart:server` at the start of `s`, if there is one.
fn mention_len(s: &str) -> Option<usize> {
    fn is_localpart(c: u8) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'=' | b'-' | b'/' | b'+')
    }
    fn is_host(c: u8) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-')
    }
    let b = s.as_bytes();
    if b.first() != Some(&b'@') {
        return None;
    }
    let mut i = 1;
    while i < b.len() && is_localpart(b[i]) {
        i += 1;
    }
    if i == 1 || b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    let host_start = i;
    while i < b.len() && is_host(b[i]) {
        i += 1;
    }
    // Trailing dots belong to the sentence: "ping @a:b.org." keeps the stop.
    while i > host_start && b[i - 1] == b'.' {
        i -= 1;
    }
    (i > host_start).then_some(i)
}

/// Bare `http(s)://…` runs → link spans (pulldown-cmark only links `[text](url)`; plain-text
/// URLs would otherwise arrive unclickable).
fn linkify_spans(spans: Vec<Span>) -> Vec<Span> {
    /// Byte offset of the next `http://` / `https://`, whichever comes first.
    fn find_url_start(s: &str) -> Option<usize> {
        match (s.find("http://"), s.find("https://")) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        // Already a link, or inside code — leave it alone.
        if span.link.is_some() || span.code {
            out.push(span);
            continue;
        }
        let text = span.text.clone();
        let mut rest = text.as_str();
        let mut split_any = false;
        while let Some(start) = find_url_start(rest) {
            let (before, from) = rest.split_at(start);
            let end = from.find(char::is_whitespace).unwrap_or(from.len());
            // Trailing punctuation belongs to the sentence, not the URL.
            let url = from[..end].trim_end_matches(|c: char| {
                matches!(
                    c,
                    '.' | ',' | ')' | ']' | '}' | '!' | '?' | ';' | ':' | '"' | '\''
                )
            });
            // A bare scheme with no host is not a link.
            if url.len() <= "https://".len() {
                let upto = start + end;
                out.push(Span {
                    text: rest[..upto].to_owned(),
                    ..span.clone()
                });
                rest = &rest[upto..];
                split_any = true;
                continue;
            }
            if !before.is_empty() {
                out.push(Span {
                    text: before.to_owned(),
                    ..span.clone()
                });
            }
            out.push(Span {
                text: url.to_owned(),
                link: Some(url.to_owned()),
                ..span.clone()
            });
            rest = &from[url.len()..];
            split_any = true;
        }
        if !rest.is_empty() || !split_any {
            out.push(Span {
                text: rest.to_owned(),
                ..span.clone()
            });
        }
    }
    out
}

/// Minimal HTML subset: `<b> <strong> <i> <em> <code> <pre> <a href> <br> <p> <blockquote>`
/// (+ mxc `<img>` → `:shortcode:`, resolved via `PackStore`). Unknown tags pass children
/// through; `<mx-reply>` quotes skipped (own reply line shown).
pub fn render_html(html: &str) -> RichMessage {
    let mut blocks = Vec::new();
    let mut spans = Vec::new();
    let mut pre_buf: Option<(String, String)> = None;
    // (bold, italic, code, link, in_mx_reply)
    let mut stack: Vec<(bool, bool, bool, Option<String>, bool)> =
        vec![(false, false, false, None, false)];
    let mut chars = html.chars().peekable();
    // Pending text + style; flushed on style change.
    let mut buf = String::new();
    let flush = |buf: &mut String,
                 spans: &mut Vec<Span>,
                 stack: &[(bool, bool, bool, Option<String>, bool)]| {
        if buf.is_empty() {
            return;
        }
        let (b, i, c, link, reply) = stack.last().cloned().unwrap_or_default();
        if !reply {
            spans.push(Span {
                text: std::mem::take(buf),
                bold: b,
                italic: i,
                code: c,
                link,
            });
        } else {
            buf.clear();
        }
    };
    while let Some(ch) = chars.next() {
        if ch != '<' {
            if pre_buf.is_some() {
                if let Some((_, code)) = pre_buf.as_mut() {
                    code.push(ch);
                }
            } else {
                buf.push(ch);
            }
            continue;
        }
        let mut tag = String::new();
        for c in chars.by_ref() {
            if c == '>' {
                break;
            }
            tag.push(c);
        }
        let tag = tag.trim();
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_lowercase();
        let self_closing = tag.ends_with('/') || name == "br" || name == "img";
        match name.as_str() {
            "pre" => {
                if closing {
                    if let Some((lang, code)) = pre_buf.take() {
                        flush(&mut buf, &mut spans, &stack);
                        if !spans.is_empty() {
                            blocks.push(Block::Spans(std::mem::take(&mut spans)));
                        }
                        blocks.push(Block::Code(CodeBlock {
                            lang,
                            code: code.trim_matches('\n').to_owned(),
                        }));
                    }
                } else {
                    flush(&mut buf, &mut spans, &stack);
                    let lang = tag_attr(tag, "class")
                        .unwrap_or_default()
                        .trim_start_matches("language-")
                        .to_owned();
                    pre_buf = Some((lang, String::new()));
                }
            }
            "mx-reply" => {
                flush(&mut buf, &mut spans, &stack);
                if closing {
                    stack.pop();
                } else {
                    let cur = stack.last().cloned().unwrap_or_default();
                    stack.push((cur.0, cur.1, cur.2, cur.3, true));
                }
            }
            "b" | "strong" | "i" | "em" | "code" | "a" => {
                flush(&mut buf, &mut spans, &stack);
                if closing {
                    stack.pop();
                } else {
                    let (b, i, c, l, r) = stack.last().cloned().unwrap_or_default();
                    let link = if name == "a" {
                        tag_attr(tag, "href")
                    } else {
                        l
                    };
                    stack.push((
                        b || name == "b" || name == "strong",
                        i || name == "i" || name == "em",
                        c || name == "code",
                        link,
                        r,
                    ));
                }
            }
            "img" => {
                // Custom emoji: <img data-mx-emoticon ... alt="shortcode">.
                flush(&mut buf, &mut spans, &stack);
                let alt = tag_attr(tag, "alt")
                    .or_else(|| tag_attr(tag, "title"))
                    .unwrap_or_default();
                if !alt.is_empty() {
                    let (b, i, c, link, reply) = stack.last().cloned().unwrap_or_default();
                    if !reply {
                        spans.push(Span {
                            text: format!(":{alt}:"),
                            bold: b,
                            italic: i,
                            code: c,
                            link,
                        });
                    }
                }
                let _ = self_closing;
            }
            "br" | "p" | "blockquote" | "ul" | "ol" | "li" | "h1" | "h2" | "h3" | "del" | "s"
            | "u" | "font" | "span" => {
                flush(&mut buf, &mut spans, &stack);
                if matches!(name.as_str(), "br" | "p" | "li") {
                    let (_, _, _, _, reply) = stack.last().cloned().unwrap_or_default();
                    if !reply {
                        spans.push(Span::plain("\n"));
                    }
                }
            }
            _ => {
                flush(&mut buf, &mut spans, &stack);
            }
        }
    }
    flush(&mut buf, &mut spans, &stack);
    if !spans.is_empty() {
        blocks.push(Block::Spans(spans));
    }
    // Decode HTML entities left in span text.
    for b in &mut blocks {
        if let Block::Spans(ss) = b {
            for s in ss {
                s.text = decode_entities(&s.text);
            }
        }
    }
    RichMessage { blocks }
}

fn tag_attr(tag: &str, name: &str) -> Option<String> {
    for pat in [format!("{name}=\""), format!("{name}='")] {
        if let Some(i) = tag.find(&pat) {
            let rest = &tag[i + pat.len()..];
            let end = rest.find(['"', '\''])?;
            return Some(rest[..end].to_owned());
        }
    }
    None
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Plain-text CommonMark subset: fenced ```blocks, inline `code`,
/// **bold**, *italic*, [links](url). Fences win over inline markup.
pub fn render_markdown_subset(body: &str) -> RichMessage {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
    let mut blocks = Vec::new();
    let mut spans = Vec::new();
    let mut bold = 0;
    let mut italic = 0;
    let mut link: Option<String> = None;
    let mut code_block: Option<(String, String)> = None;
    #[allow(unused_mut)]
    let mut push_text =
        |text: &str, spans: &mut Vec<Span>, bold: i32, italic: i32, link: &Option<String>| {
            if text.is_empty() {
                return;
            }
            spans.push(Span {
                text: text.to_owned(),
                bold: bold > 0,
                italic: italic > 0,
                code: false,
                link: link.clone(),
            });
        };
    for ev in Parser::new(body) {
        match ev {
            Event::Start(Tag::Strong) => {
                if code_block.is_none() {
                    bold += 1;
                }
            }
            Event::Start(Tag::Emphasis) => {
                if code_block.is_none() {
                    italic += 1;
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                if code_block.is_none() {
                    link = Some(dest_url.into_string());
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                if !spans.is_empty() {
                    blocks.push(Block::Spans(std::mem::take(&mut spans)));
                }
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code_block = Some((lang, String::new()));
            }
            Event::End(TagEnd::Strong) => bold = (bold - 1).max(0),
            Event::End(TagEnd::Emphasis) => italic = (italic - 1).max(0),
            Event::End(TagEnd::Link) => link = None,
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, code)) = code_block.take() {
                    blocks.push(Block::Code(CodeBlock {
                        lang,
                        code: code.trim_matches('\n').to_owned(),
                    }));
                }
            }
            Event::Text(t) => {
                if let Some((_, code)) = code_block.as_mut() {
                    code.push_str(&t);
                } else {
                    push_text(&t, &mut spans, bold, italic, &link);
                }
            }
            Event::Code(c) => {
                if code_block
                    .as_mut()
                    .map(|(_, code)| code.push_str(&c))
                    .is_none()
                {
                    spans.push(Span {
                        text: c.into_string(),
                        bold: bold > 0,
                        italic: italic > 0,
                        code: true,
                        link: link.clone(),
                    });
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if code_block
                    .as_mut()
                    .map(|(_, code)| code.push('\n'))
                    .is_none()
                {
                    push_text("\n", &mut spans, bold, italic, &link);
                }
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                push_text(&h, &mut spans, bold, italic, &link);
            }
            _ => {}
        }
    }
    if !spans.is_empty() {
        blocks.push(Block::Spans(spans));
    }
    if blocks.is_empty() {
        blocks.push(Block::Spans(vec![Span::plain(body)]));
    }
    RichMessage { blocks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_url_becomes_a_link() {
        let m = render_message("see https://example.com/x for details", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        let link = spans
            .iter()
            .find(|s| s.link.is_some())
            .expect("bare URL must linkify");
        assert_eq!(link.text, "https://example.com/x");
        assert_eq!(link.link.as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_url() {
        let m = render_message("go to https://example.com.", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        let link = spans.iter().find(|s| s.link.is_some()).expect("linkified");
        assert_eq!(link.link.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn code_spans_are_not_linkified() {
        let m = render_message("`https://example.com`", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        assert!(
            spans.iter().all(|s| s.link.is_none()),
            "a URL inside code must stay literal"
        );
    }

    #[test]
    fn bare_mxid_becomes_a_mention_link() {
        let m = render_message("ping @alice:example.org about it", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        let pill = spans
            .iter()
            .find(|s| s.link.is_some())
            .expect("bare mxid must linkify");
        assert_eq!(pill.text, "@alice:example.org");
        assert_eq!(
            pill.link.as_deref(),
            Some("https://matrix.to/#/@alice:example.org")
        );
        // Keep surrounding text.
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "ping @alice:example.org about it");
    }

    #[test]
    fn mention_stops_before_sentence_punctuation() {
        let m = render_message("ask @bob:matrix.org.", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        let pill = spans.iter().find(|s| s.link.is_some()).expect("linkified");
        assert_eq!(pill.text, "@bob:matrix.org");
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "ask @bob:matrix.org.", "no text may be lost");
    }

    #[test]
    fn mxid_inside_a_url_is_left_to_the_url() {
        let m = render_message("https://matrix.to/#/@a:b.org now", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        let links: Vec<_> = spans.iter().filter(|s| s.link.is_some()).collect();
        assert_eq!(links.len(), 1, "URL wins; no nested mention span");
        assert_eq!(links[0].text, "https://matrix.to/#/@a:b.org");
    }

    #[test]
    fn email_like_text_is_not_a_mention() {
        let m = render_message("mail bob@example.org please", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        assert!(
            spans.iter().all(|s| s.link.is_none()),
            "an @ mid-word is not an mxid mention"
        );
    }

    #[test]
    fn a_url_is_one_span_not_split_on_its_colon() {
        // Spans split on ':' for `:shortcode:` emoji; a URL must stay one span or it renders
        // with a gap ("https" + "://x").
        let m = render_message("see https://google.com ok", None);
        let Block::Spans(spans) = &m.blocks[0] else {
            panic!("expected spans");
        };
        let links: Vec<_> = spans.iter().filter(|s| s.link.is_some()).collect();
        assert_eq!(links.len(), 1, "the URL must be exactly one span");
        assert_eq!(links[0].text, "https://google.com");
    }

    #[test]
    fn html_bold_and_link() {
        let m = render_html("<b>hi</b> <a href=\"https://x\">yo</a>");
        assert_eq!(m.blocks.len(), 1);
        let Block::Spans(ss) = &m.blocks[0] else {
            panic!("spans")
        };
        assert!(ss.iter().any(|s| s.text == "hi" && s.bold));
        assert!(ss
            .iter()
            .any(|s| s.text == "yo" && s.link.as_deref() == Some("https://x")));
    }

    #[test]
    fn html_pre_becomes_code_block() {
        let m = render_html("<pre><code class=\"language-rust\">let x = 1;</code></pre>");
        assert!(matches!(m.blocks.last(), Some(Block::Code(_))));
    }

    #[test]
    fn html_skips_mx_reply() {
        let m = render_html("<mx-reply><blockquote>quoted</blockquote></mx-reply>real");
        let texts: Vec<_> = m
            .blocks
            .iter()
            .flat_map(|b| match b {
                Block::Spans(s) => s.clone(),
                _ => vec![],
            })
            .collect();
        assert!(texts.iter().all(|s| !s.text.contains("quoted")));
        assert!(texts.iter().any(|s| s.text.contains("real")));
    }

    #[test]
    fn markdown_code_block_and_inline() {
        let m = render_markdown_subset("hey `x` here\n```rust\nlet y = 2;\n```");
        assert!(m.blocks.iter().any(|b| matches!(b, Block::Code(_))));
        let inline_code = m
            .blocks
            .iter()
            .flat_map(|b| match b {
                Block::Spans(s) => s.clone(),
                _ => vec![],
            })
            .any(|s| s.text == "x" && s.code);
        assert!(inline_code);
    }

    #[test]
    fn prefers_html_over_plain() {
        let m = render_message("plain", Some("<b>rich</b>"));
        let Block::Spans(ss) = &m.blocks[0] else {
            panic!("spans")
        };
        assert!(ss.iter().any(|s| s.bold));
    }
}

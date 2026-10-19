//! Matrix-safe Markdown with bounded events and stable legacy chunk identities.
use ammonia::{Builder, UrlRelative};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};
use serde_json::{Value, json};
use std::{collections::HashMap, ops::Range, sync::LazyLock};

const CONTENT_LIMIT: usize = 48_000;
static SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    // @implements url:https://spec.matrix.org/v1.16/client-server-api/#mroommessage-msgtypes -- formatted_body HTML allowlist
    builder
        .tags(
            [
                "p",
                "br",
                "strong",
                "b",
                "em",
                "i",
                "del",
                "s",
                "code",
                "pre",
                "blockquote",
                "ul",
                "ol",
                "li",
                "h1",
                "h2",
                "h3",
                "h4",
                "h5",
                "h6",
                "hr",
                "a",
                "table",
                "thead",
                "tbody",
                "tr",
                "th",
                "td",
            ]
            .into_iter()
            .collect(),
        )
        .generic_attributes(Default::default())
        .tag_attributes(HashMap::from([
            ("a", ["href", "title"].into_iter().collect()),
            ("code", ["class"].into_iter().collect()),
            ("ol", ["start"].into_iter().collect()),
        ]))
        .url_schemes(["http", "https", "mailto", "matrix"].into_iter().collect())
        .url_relative(UrlRelative::Deny)
        .link_rel(None);
    builder
});

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

pub struct Chunk {
    pub source_index: usize,
    pub continuation: usize,
    pub content: Value,
}

struct Fence<'a> {
    range: Range<usize>,
    opening: &'a str,
}

fn content(text: &str, start: usize, end: usize, fences: &[Fence<'_>]) -> Value {
    let prefix = fences
        .iter()
        .find(|f| f.range.start < start && start < f.range.end)
        .map(|f| f.opening);
    let input = prefix.map(|prefix| format!("{prefix}\n{}", &text[start..end]));
    let input = input.as_deref().unwrap_or(&text[start..end]);
    // Raw HTML is text; agent output cannot introduce executable HTML or images.
    let events = Parser::new_ext(input, options()).map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }),
        Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
        other => other,
    });
    let mut rendered = String::new();
    html::push_html(&mut rendered, events);
    let formatted = SANITIZER.clean(&rendered).to_string();
    // @implements url:https://spec.matrix.org/v1.16/client-server-api/#mtext -- retain the original text fallback
    json!({"msgtype":"m.text","body":&text[start..end],"format":"org.matrix.custom.html","formatted_body":formatted})
}

fn split(text: &str, range: Range<usize>, fences: &[Fence<'_>], output: &mut Vec<Value>) {
    let value = content(text, range.start, range.end, fences);
    if serde_json::to_vec(&value)
        .expect("JSON value serializes")
        .len()
        <= CONTENT_LIMIT
    {
        output.push(value);
        return;
    }
    let mut middle = range.start + (range.end - range.start) / 2;
    while !text.is_char_boundary(middle) {
        middle -= 1;
    }
    if let Some(newline) = text[range.start..middle].rfind('\n') {
        middle = range.start + newline + 1;
    }
    // One Unicode scalar cannot approach the event budget.
    assert!(range.start < middle && middle < range.end);
    split(text, range.start..middle, fences, output);
    split(text, middle..range.end, fences, output);
}

pub fn chunks(text: &str) -> Vec<Chunk> {
    let fences = Parser::new_ext(text, options())
        .into_offset_iter()
        .filter_map(|(event, range)| {
            if let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) = event {
                let opening = text[range.clone()].lines().next()?;
                Some(Fence { range, opening })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut offset = 0;
    for (source_index, chunk) in crate::service::text_chunks(text, 40_000)
        .into_iter()
        .enumerate()
    {
        let mut parts = Vec::new();
        split(text, offset..offset + chunk.len(), &fences, &mut parts);
        output.extend(
            parts
                .into_iter()
                .enumerate()
                .map(|(continuation, content)| Chunk {
                    source_index,
                    continuation,
                    content,
                }),
        );
        offset += chunk.len();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_common_agent_markdown_with_an_exact_text_fallback() {
        let text = "# Heading\n\n**bold**, *italic*, ~~removed~~ and `code`\n\n- one\n- two\n\n> quote\n\n```rust\nlet x = 1 < 2;\n```\n\n[link](https://example.test)\n\n| A | B |\n|---|---|\n| 1 | 2 |";
        let chunks = chunks(text);
        assert_eq!(chunks.len(), 1);
        let value = &chunks[0].content;
        assert_eq!(value["body"], text);
        assert_eq!(value["format"], "org.matrix.custom.html");
        let html = value["formatted_body"].as_str().unwrap();
        for expected in [
            "<h1>",
            "<strong>bold</strong>",
            "<em>italic</em>",
            "<del>removed</del>",
            "<code>code</code>",
            "<ul>",
            "<blockquote>",
            "<pre><code class=\"language-rust\">",
            "1 &lt; 2",
            "href=\"https://example.test\"",
            "<table>",
        ] {
            assert!(html.contains(expected), "missing {expected}: {html}");
        }
    }

    #[test]
    fn unsafe_html_urls_and_external_images_cannot_enter_formatted_messages() {
        let value = &chunks("<script>alert(1)</script>\n\n[x](javascript:alert(1)) ![alt](https://example.test/image.png) <img src=x onerror=alert(1)>\n\n[safe](https://example.test)")[0].content;
        let html = value["formatted_body"].as_str().unwrap();
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("href=\"https://example.test\""));
    }

    #[test]
    fn long_code_and_escaped_unicode_keep_valid_bounded_events_and_all_original_text() {
        for text in [
            format!("```rust\n{}\n```", "let text = \"<&雪>\";\n".repeat(5000)),
            "\u{1}雪<&".repeat(20_000),
        ] {
            let parts = chunks(&text);
            assert!(parts.len() > 1);
            assert_eq!(
                parts
                    .iter()
                    .map(|p| p.content["body"].as_str().unwrap())
                    .collect::<String>(),
                text
            );
            for part in parts {
                assert!(serde_json::to_vec(&part.content).unwrap().len() <= CONTENT_LIMIT);
                if text.starts_with("```rust") {
                    assert!(
                        part.content["formatted_body"]
                            .as_str()
                            .unwrap()
                            .contains("<pre><code class=\"language-rust\">")
                    );
                }
            }
        }
    }
}

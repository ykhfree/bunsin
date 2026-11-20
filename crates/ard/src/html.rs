//! In-page markup (§5.1): `<link rel="ard">` in the head of a page, and entries in JSON-LD blocks.
//!
//! The html5ever tokenizer reads the page. A tokenizer alone does not know that `<script>` holds
//! raw text, so the sink tells it, as a tree builder would. Then a link in a comment, a script
//! string or a `<style>` is text, not a tag.

use crate::jsonld::BASE_CONTEXT_URL;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, CharacterTokens, EndTag, StartTag, Tag, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use serde_json::{Map, Value};

/// The most JSON-LD blocks that one page gives.
pub const MAX_BLOCKS: usize = 20;
/// The largest JSON-LD block, in bytes. A larger block is skipped.
pub const MAX_BLOCK_BYTES: usize = 64 * 1024;

/// What a page offers to ARD.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Markup {
    /// The `href` of each `<link rel="ard">` before the `<body>` start tag, as written.
    pub links: Vec<String>,
    /// The text of each `<script type="application/ld+json">`, in page order.
    pub blocks: Vec<String>,
}

/// Reads the links and JSON-LD blocks of a page.
///
/// Without a tree builder, "the head" is everything before the `<body>` start tag. A page without
/// that tag gives every `<link rel="ard">` outside raw text.
pub fn read_markup(html: &str) -> Markup {
    let mut queue = BufferQueue::default();
    queue.push_back(StrTendril::from_slice(html));
    let mut tokenizer = Tokenizer::new(Sink::default(), TokenizerOpts::default());
    // The sink never asks to run a script, so the tokenizer reads the whole input at once.
    let _ = tokenizer.feed(&mut queue);
    tokenizer.end();
    tokenizer.sink.markup
}

#[derive(Default)]
struct Sink {
    markup: Markup,
    in_body: bool,
    /// The text of the JSON-LD block that is open, if any.
    block: Option<String>,
}

fn attribute<'a>(tag: &'a Tag, name: &str) -> Option<&'a str> {
    tag.attrs
        .iter()
        .find(|attr| &*attr.name.local == name)
        .map(|attr| &*attr.value)
}

impl Sink {
    fn start(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        match &*tag.name {
            "body" => self.in_body = true,
            "link" if !self.in_body => {
                // `rel` is a list of tokens separated by spaces, in any case.
                let ard = attribute(tag, "rel")
                    .is_some_and(|rel| rel.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("ard")));
                let href = attribute(tag, "href").map(str::trim).filter(|href| !href.is_empty());
                if let (true, Some(href)) = (ard, href) {
                    self.markup.links.push(href.to_owned());
                }
            }
            "script" => {
                let json_ld = attribute(tag, "type").is_some_and(|t| {
                    t.split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("application/ld+json")
                });
                if json_ld && self.markup.blocks.len() < MAX_BLOCKS {
                    self.block = Some(String::new());
                }
                return TokenSinkResult::RawData(RawKind::ScriptData);
            }
            // The elements whose content HTML reads as text (a browser with scripts on).
            "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
                return TokenSinkResult::RawData(RawKind::Rawtext);
            }
            "title" | "textarea" => return TokenSinkResult::RawData(RawKind::Rcdata),
            "plaintext" => return TokenSinkResult::Plaintext,
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&mut self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            TagToken(tag) if tag.kind == StartTag => return self.start(&tag),
            TagToken(tag) if tag.kind == EndTag && &*tag.name == "script" => {
                if let Some(block) = self.block.take()
                    && block.len() <= MAX_BLOCK_BYTES
                {
                    self.markup.blocks.push(block);
                }
            }
            CharacterTokens(text) => {
                // Text past the limit is not kept: the block is skipped at its end tag.
                if let Some(block) = self.block.as_mut()
                    && block.len() <= MAX_BLOCK_BYTES
                {
                    block.push_str(&text);
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// The entry candidates in JSON-LD blocks.
///
/// A candidate is an object, an array element or a `@graph` element that names the base context in
/// its `@context`, or that has no `@context` and has `identifier`, `displayName` and `type`.
/// A `@graph` element without its own `@context` takes the one of the object around it.
/// A block that is not JSON gives nothing.
pub fn markup_entries(blocks: &[String]) -> Vec<Value> {
    let mut entries = Vec::new();
    for block in blocks {
        let Ok(value) = serde_json::from_str::<Value>(block) else {
            continue;
        };
        let nodes = match &value {
            Value::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for node in nodes {
            let Value::Object(object) = node else { continue };
            match object.get("@graph") {
                Some(Value::Array(graph)) => {
                    for item in graph {
                        if let Value::Object(item) = item {
                            entries.extend(candidate(item, object.get("@context")));
                        }
                    }
                }
                _ => entries.extend(candidate(object, None)),
            }
        }
    }
    entries
}

fn candidate(node: &Map<String, Value>, outer: Option<&Value>) -> Option<Value> {
    let context = node.get("@context").or(outer);
    let names_base = |context: &Value| match context {
        Value::String(url) => url == BASE_CONTEXT_URL,
        Value::Array(parts) => parts.iter().any(|p| p.as_str() == Some(BASE_CONTEXT_URL)),
        _ => false,
    };
    let chosen = match context {
        Some(context) => names_base(context),
        None => ["identifier", "displayName", "type"]
            .iter()
            .all(|t| node.contains_key(*t)),
    };
    if !chosen {
        return None;
    }
    let mut node = node.clone();
    if let (false, Some(outer)) = (node.contains_key("@context"), outer) {
        node.insert("@context".into(), outer.clone());
    }
    Some(Value::Object(node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn head_links_and_json_ld_blocks_are_found() {
        let page = r#"<!doctype html><html><head>
            <LINK REL="alternate ARD" href=" /catalog.json ">
            <link rel="stylesheet" href="/a.css">
            <link rel="ard">
            <script type="application/ld+json; charset=utf-8">{"a": "</b>"}</script>
            </head><body>
            <link rel="ard" href="/late.json">
            <script type="APPLICATION/LD+JSON">[1]</script>
            <script>var x = 1;</script>
            </body></html>"#;
        let markup = read_markup(page);
        // A link in the body and a link without `href` are not counted.
        assert_eq!(markup.links, ["/catalog.json"]);
        // Blocks count anywhere in the page. A plain script is not a block.
        assert_eq!(markup.blocks, [r#"{"a": "</b>"}"#, "[1]"]);
    }

    #[test]
    fn commented_links_and_blocks_are_not_found() {
        let page = r#"<html><head>
            <!-- <link rel="ard" href="/commented.json"> -->
            <!-- <script type="application/ld+json">{"commented": true}</script> -->
            <script>var s = '<link rel="ard" href="/in-script.json">';
              var t = '<script type="application/ld+json">{"in": "script"}<\/script>';</script>
            <style>/* <link rel="ard" href="/in-style.json"> */</style>
            <title><link rel="ard" href="/in-title.json"></title>
            <noscript><link rel="ard" href="/in-noscript.json"></noscript>
            <textarea><script type="application/ld+json">{"in": "textarea"}</script></textarea>
            <link rel="ard" href="/real.json">
            </head></html>"#;
        let markup = read_markup(page);
        assert_eq!(markup.links, ["/real.json"]);
        assert!(markup.blocks.is_empty(), "{:?}", markup.blocks);
    }

    #[test]
    fn blocks_are_limited_in_number_and_size() {
        let block = |n: usize| format!(r#"<script type="application/ld+json">{{"n": {n}}}</script>"#);
        let big = format!(
            r#"<script type="application/ld+json">"{}"</script>"#,
            "x".repeat(MAX_BLOCK_BYTES)
        );
        let page: String = std::iter::once(big).chain((0..25).map(block)).collect();
        let markup = read_markup(&page);
        // The large block is skipped, and only the first 20 of the others are kept.
        assert_eq!(markup.blocks.len(), MAX_BLOCKS);
        assert_eq!(markup.blocks[0], r#"{"n": 0}"#);
        assert_eq!(markup.blocks[19], r#"{"n": 19}"#);
    }

    #[test]
    fn only_ard_nodes_of_a_block_are_candidates() {
        let base = BASE_CONTEXT_URL;
        let blocks = [
            // A product description of another vocabulary.
            json!({"@context": "https://schema.org", "@type": "Product", "name": "x"}).to_string(),
            // A plain entry without a context.
            json!({"identifier": "urn:air:a.com:x", "displayName": "X", "type": "text/plain", "url": "https://a.com"})
                .to_string(),
            // Without a context, all three core terms are needed.
            json!({"identifier": "urn:air:a.com:y", "displayName": "Y"}).to_string(),
            // An array, with the base context in an array.
            json!([{"@context": [base, {"acme": "https://acme.com/v#"}], "identifier": "urn:air:a.com:z"}, 5])
                .to_string(),
            // A graph: its elements take the outer context.
            json!({"@context": base, "@graph": [{"identifier": "urn:air:a.com:g1"}, {"@context": "https://schema.org", "identifier": "urn:air:a.com:g2"}]})
                .to_string(),
            json!({"@context": "https://schema.org", "@graph": [{"identifier": "urn:air:a.com:g3"}]}).to_string(),
            "not json".to_owned(),
        ];
        let found = markup_entries(&blocks);
        let ids: Vec<&str> = found.iter().map(|e| e["identifier"].as_str().unwrap()).collect();
        assert_eq!(ids, ["urn:air:a.com:x", "urn:air:a.com:z", "urn:air:a.com:g1"]);
        assert_eq!(found[2]["@context"], json!(base));
        assert!(found[0].get("@context").is_none());
    }
}

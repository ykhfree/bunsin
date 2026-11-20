//! The minimal JSON-LD expansion that ARD needs (§4.1, §5.3.1).
//!
//! Filters and facets compare the IRIs of top-level terms only. So this module turns each
//! top-level key into an IRI and leaves every value as it is. It is not a JSON-LD processor.
//!
//! The base context is a copy inside this crate (`ard.context.jsonld`), unchanged. Source:
//! <https://github.com/ards-project/ard-spec>, file `spec/schemas/ard.context.jsonld`, tree
//! f073b1ea9b01a5f068d0b0986426f2ea49fa30b2 (blob de1ea70b). Copied on 2026-11-20.
//!
//! A remote context is never fetched: only the base URL is known. A context may add terms and
//! prefixes, each an absolute IRI, a compact IRI with a known prefix, or `{"@id": IRI}`. Anything
//! else is refused, so that a publisher cannot change what a core term means. A core term must
//! also be written with its own name, because the entry rules read it by that name.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The URL of the ARD base context (§4.1).
pub const BASE_CONTEXT_URL: &str = "https://agenticresourcediscovery.org/context/v1";

/// The default namespace of the core terms (§4.1).
pub const ARD_NAMESPACE: &str = "https://agenticresourcediscovery.org/ns#";

/// The base context, as published at [`BASE_CONTEXT_URL`].
const BASE_CONTEXT: &str = include_str!("ard.context.jsonld");

/// The members that a term definition object may have. They shape values, which stay unchanged here.
const DEFINITION_MEMBERS: [&str; 3] = ["@id", "@type", "@container"];

/// A context that this module does not support. The text says what is unsupported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unsupported @context: {0}")]
pub struct UnsupportedContext(pub String);

fn unsupported(text: impl Into<String>) -> UnsupportedContext {
    UnsupportedContext(text.into())
}

/// An active context: the base context and the definitions layered on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// Term or prefix to IRI. A keyword alias maps to the keyword, such as `id` to `@id`.
    terms: BTreeMap<String, String>,
    vocab: String,
}

impl Context {
    /// The base context alone.
    pub fn base() -> &'static Context {
        static BASE: OnceLock<Context> = OnceLock::new();
        BASE.get_or_init(|| {
            let file: Value = serde_json::from_str(BASE_CONTEXT).expect("the bundled base context is JSON");
            let Some(Value::Object(definitions)) = file.get("@context") else {
                panic!("the bundled base context has no @context object");
            };
            let mut context = Context {
                terms: BTreeMap::new(),
                vocab: String::new(),
            };
            context
                .define(definitions, true)
                .expect("the bundled base context is supported");
            context
        })
    }

    /// Layers `local` on this context. `local` is the value of an `@context` member, if any.
    ///
    /// Supported: the base URL, an object of term and prefix definitions, and an array of these.
    /// Refused: other remote contexts, `null`, keywords such as `@vocab`, `@base`, `@import` and
    /// `@protected`, scoped contexts, `@reverse`, `@nest`, and a new definition of a base term.
    pub fn with(&self, local: Option<&Value>) -> Result<Context, UnsupportedContext> {
        let mut context = self.clone();
        let parts = match local {
            None => return Ok(context),
            Some(Value::Array(parts)) => parts.as_slice(),
            Some(single) => std::slice::from_ref(single),
        };
        for part in parts {
            match part {
                Value::String(url) if url == BASE_CONTEXT_URL => {}
                Value::String(url) => return Err(unsupported(format!("the remote context `{url}`"))),
                Value::Object(definitions) => context.define(definitions, false)?,
                other => return Err(unsupported(format!("the context value `{other}`"))),
            }
        }
        Ok(context)
    }

    /// The IRI of a top-level key: by term, then compact IRI, then absolute IRI, then `@vocab`.
    /// A keyword, or a key that aliases one, gives the keyword.
    pub fn expand_key(&self, key: &str) -> String {
        if key.starts_with('@') {
            return key.to_owned();
        }
        if let Some(iri) = self.terms.get(key) {
            return iri.clone();
        }
        if key.contains(':') {
            return self.compact(key).unwrap_or_else(|| key.to_owned());
        }
        format!("{}{key}", self.vocab)
    }

    /// Expands `prefix:suffix` if `prefix` is a term of this context that is not a keyword alias.
    fn compact(&self, value: &str) -> Option<String> {
        let (prefix, suffix) = value.split_once(':')?;
        if suffix.starts_with("//") {
            return None;
        }
        let base = self.terms.get(prefix).filter(|iri| !iri.starts_with('@'))?;
        Some(format!("{base}{suffix}"))
    }

    /// Adds the definitions of one context object. Only the bundled base may set `@vocab`,
    /// alias a keyword, or define a base term.
    fn define(&mut self, definitions: &Map<String, Value>, base: bool) -> Result<(), UnsupportedContext> {
        let base_terms = (!base).then(|| &Context::base().terms);
        let mut found = Vec::new();
        for (key, definition) in definitions {
            if key.starts_with('@') {
                match (key.as_str(), definition) {
                    ("@version", _) => {}
                    ("@vocab", Value::String(vocab)) if base => self.vocab = vocab.clone(),
                    _ => return Err(unsupported(format!("the keyword `{key}`"))),
                }
                continue;
            }
            if base_terms.is_some_and(|terms| terms.contains_key(key)) {
                return Err(unsupported(format!("a new definition of the base term `{key}`")));
            }
            let raw = match definition {
                Value::String(iri) => iri,
                Value::Object(members) => {
                    if let Some(member) = members.keys().find(|m| !DEFINITION_MEMBERS.contains(&m.as_str())) {
                        return Err(unsupported(format!("`{member}` in the definition of `{key}`")));
                    }
                    match members.get("@id") {
                        Some(Value::String(iri)) => iri,
                        _ => return Err(unsupported(format!("the definition of `{key}` has no `@id` string"))),
                    }
                }
                _ => return Err(unsupported(format!("the definition of `{key}`"))),
            };
            found.push((key, raw));
        }
        // A definition can use a prefix of the same object, whatever the key order.
        let mut terms = Vec::new();
        for (key, raw) in &found {
            if raw.starts_with('@') && base {
                terms.push(((*key).clone(), (*raw).clone()));
                continue;
            }
            let iri = self
                .compact(raw)
                .or_else(|| local_compact(&found, raw))
                .unwrap_or_else(|| (*raw).clone());
            if iri.starts_with('@') || url::Url::parse(&iri).is_err() {
                return Err(unsupported(format!(
                    "the definition of `{key}` is not an absolute IRI: `{raw}`"
                )));
            }
            terms.push(((*key).clone(), iri));
        }
        self.terms.extend(terms);
        Ok(())
    }
}

/// Expands `prefix:suffix` with a prefix defined as an absolute IRI in the same context object.
fn local_compact(found: &[(&String, &String)], value: &str) -> Option<String> {
    let (prefix, suffix) = value.split_once(':')?;
    if suffix.starts_with("//") {
        return None;
    }
    let (_, base) = found.iter().find(|(key, _)| key.as_str() == prefix)?;
    Some(format!("{base}{suffix}"))
}

/// Expands the top-level keys of a node with the base context and its own `@context`.
///
/// Returns each term's IRI with its value unchanged. Keywords such as `@id` are left out.
///
/// Fails if the context is unsupported, or if a key spells a base term another way, such as
/// `ard:trustManifest`. The entry rules read a base term by its own name, so another spelling
/// would pass by them. Fails also if two keys name the same IRI: one value would hide the other.
pub fn expand_terms(node: &Map<String, Value>) -> Result<Map<String, Value>, UnsupportedContext> {
    let base = Context::base();
    let context = base.with(node.get("@context"))?;
    let mut terms = Map::new();
    for (key, value) in node {
        let iri = context.expand_key(key);
        if iri.starts_with('@') {
            continue;
        }
        if let Some((name, _)) = base
            .terms
            .iter()
            .find(|(name, base_iri)| **base_iri == iri && *name != key)
        {
            return Err(unsupported(format!(
                "the key `{key}` spells the base term `{name}` another way"
            )));
        }
        if terms.insert(iri.clone(), value.clone()).is_some() {
            return Err(unsupported(format!("two keys expand to the same IRI `{iri}`")));
        }
    }
    Ok(terms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns(term: &str) -> String {
        format!("{ARD_NAMESPACE}{term}")
    }

    fn node(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else {
            panic!("not an object")
        };
        map
    }

    #[test]
    fn the_base_context_expands_core_terms_and_entry_prefixes() {
        // The namespaced example of §4.4.
        let entry =
            node(serde_json::from_str(include_str!("../tests/fixtures/spec_4_4_namespaced_entry.json")).unwrap());
        let terms = expand_terms(&entry).unwrap();
        assert_eq!(terms[&ns("identifier")], json!("urn:air:acme.com:server:weather"));
        // `type` is the media type term, not `ns#type`.
        assert_eq!(terms[&ns("mediaType")], json!("application/mcp-server-card+json"));
        assert!(!terms.contains_key(&ns("type")));
        assert_eq!(terms[&ns("representativeQueries")].as_array().unwrap().len(), 2);
        assert_eq!(terms["https://acme.com/vocab#serviceTier"], json!("enterprise"));
        assert_eq!(terms["https://acme.com/vocab#region"], json!(["us-east", "eu-west"]));
        // `@context` is a keyword, not a term.
        assert!(terms.keys().all(|iri| !iri.starts_with('@')));
        assert_eq!(terms.len(), 9);

        // A plain key that no context defines falls into the default namespace. `id` aliases `@id`.
        let terms = expand_terms(&node(json!({
            "@context": ["https://agenticresourcediscovery.org/context/v1", {"okf": {"@id": "https://openknowledgeformat.org/ns#"}}],
            "id": "urn:air:acme.com:x",
            "okf:taxonomy": ["us-gaap"],
            "colour": "blue",
            "https://example.org/terms#rank": 3,
        })))
        .unwrap();
        assert_eq!(terms["https://openknowledgeformat.org/ns#taxonomy"], json!(["us-gaap"]));
        assert_eq!(terms[&ns("colour")], json!("blue"));
        assert_eq!(terms["https://example.org/terms#rank"], json!(3));
        assert_eq!(terms.len(), 3);
    }

    #[test]
    fn a_query_prefix_matches_an_entry_that_chose_another_prefix() {
        let query = Context::base()
            .with(Some(&json!({"okf": "https://openknowledgeformat.org/ns#"})))
            .unwrap();
        let entry = node(json!({
            "@context": {"openknowledge": "https://openknowledgeformat.org/ns#", "tax": "openknowledge:taxonomy"},
            "openknowledge:taxonomy": ["us-gaap"],
        }));
        let terms = expand_terms(&entry).unwrap();
        let key = query.expand_key("okf:taxonomy");
        assert_eq!(key, "https://openknowledgeformat.org/ns#taxonomy");
        assert_eq!(terms[&key], json!(["us-gaap"]));
        // The query's prefix means nothing to the base context alone, so it stays as it is.
        assert_eq!(Context::base().expand_key("okf:taxonomy"), "okf:taxonomy");
        // Core terms need no query context.
        assert_eq!(query.expand_key("tags"), ns("tags"));
        assert_eq!(query.expand_key("ard:tags"), ns("tags"));
    }

    #[test]
    fn a_core_term_redefinition_or_remote_context_is_refused() {
        let refused = [
            json!("https://schema.org"),
            json!([
                "https://agenticresourcediscovery.org/context/v1",
                "https://evil.example/ctx"
            ]),
            json!(null),
            json!([null]),
            json!({"displayName": "https://evil.example/name"}),
            json!({"ard": "https://evil.example/ns#"}),
            json!({"type": {"@id": "https://schema.org/type"}}),
            json!({"@vocab": "https://evil.example/"}),
            json!({"@base": "https://evil.example/"}),
            json!({"@import": "https://evil.example/ctx"}),
            json!({"@protected": true}),
            json!({"acme": {"@id": "https://acme.com/v#", "@context": {"x": "https://y.example/"}}}),
            json!({"acme": {"@reverse": "https://acme.com/v#parent"}}),
            json!({"acme": {"@id": "https://acme.com/v#", "@nest": "a"}}),
            json!({"acme": "not an iri"}),
            json!({"acme": "@id"}),
            json!({"acme": 5}),
        ];
        for context in refused {
            let entry = node(json!({"@context": context.clone(), "identifier": "urn:air:acme.com:x"}));
            let error = expand_terms(&entry).unwrap_err();
            assert!(
                error.to_string().starts_with("unsupported @context: "),
                "{context}: {error}"
            );
        }
        // A base term spelled another way is refused, alone or beside its own name.
        for key in [
            "ard:trustManifest",
            "https://agenticresourcediscovery.org/ns#trustManifest",
            "ard:mediaType",
        ] {
            let aliased = node(json!({"identifier": "urn:air:acme.com:x", key: {"identity": "spiffe://google.com/x"}}));
            let error = expand_terms(&aliased).unwrap_err();
            assert!(error.0.contains("spells the base term"), "{key}: {error}");
        }
        let twice = node(json!({"identifier": "urn:air:acme.com:x", "ard:identifier": "urn:air:google.com:x"}));
        assert!(expand_terms(&twice).unwrap_err().0.contains("spells the base term"));
        // Two keys for the same IRI outside the base terms are refused, so one value cannot hide another.
        let twice = node(json!({"colour": "blue", "ard:colour": "red"}));
        assert!(expand_terms(&twice).unwrap_err().0.contains("same IRI"));
        // The base URL, with or without an array, and a version marker are accepted.
        for context in [
            json!("https://agenticresourcediscovery.org/context/v1"),
            json!(["https://agenticresourcediscovery.org/context/v1"]),
            json!({"@version": 1.1, "acme": {"@id": "https://acme.com/v#", "@type": "@id", "@container": "@set"}}),
        ] {
            let entry = node(json!({"@context": context.clone(), "identifier": "urn:air:acme.com:x"}));
            assert!(expand_terms(&entry).is_ok(), "{context}");
        }
    }
}

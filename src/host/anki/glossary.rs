// SPDX-License-Identifier: GPL-3.0-or-later
//! Glossaries as Anki fields: the extension's `createAnkiDefinitionRenderer`
//! (`anki-glossary.js`), with the Anki layout of `HDGlossary`'s structured
//! content (`appendTextOnlyGlossary`) and its dictionary styles.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use super::compact;
use super::css::{self, Selector};
use super::dom::{Element, Node, escape_text};
use super::gateway::{Failure, Result};
use crate::host::js;

const MAX_STRUCTURED_NODES: usize = 1_048_576;
const MAX_LOCATION_SEGMENTS: usize = 64;
const MAX_DATA_ATTRIBUTES: usize = 64;
const MAX_DATA_KEY_LENGTH: usize = 64;
const MAX_DATA_VALUE_LENGTH: usize = 4096;
const MAX_LOOKUP_TEXT_BYTES: usize = 4 * 1024;
const MAX_DICTIONARY_STYLE_BYTES: usize = 256 * 1024;
const MAX_DICTIONARY_STYLES_BYTES: usize = 2 * 1024 * 1024;
/// How deeply structured content may nest here (the extension walks it without recursion).
const MAX_DEPTH: usize = 4096;

const ALLOWED_TAGS: [&str; 27] = [
    "a", "br", "code", "details", "div", "em", "img", "li", "ol", "p", "rp", "rt", "ruby", "small", "span", "strong", "sub", "summary",
    "sup", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "ul",
];
const IGNORED_TAGS: [&str; 10] = ["audio", "button", "canvas", "iframe", "input", "script", "source", "style", "svg", "video"];
const STYLE_PROPERTIES: [(&str, &str); 32] = [
    ("fontStyle", "font-style"),
    ("fontWeight", "font-weight"),
    ("fontSize", "font-size"),
    ("color", "color"),
    ("background", "background"),
    ("backgroundColor", "background-color"),
    ("verticalAlign", "vertical-align"),
    ("textAlign", "text-align"),
    ("textEmphasis", "text-emphasis"),
    ("textShadow", "text-shadow"),
    ("textDecorationLine", "text-decoration"),
    ("textDecorationStyle", "text-decoration-style"),
    ("textDecorationColor", "text-decoration-color"),
    ("borderColor", "border-color"),
    ("borderStyle", "border-style"),
    ("borderRadius", "border-radius"),
    ("borderWidth", "border-width"),
    ("clipPath", "clip-path"),
    ("margin", "margin"),
    ("marginTop", "margin-top"),
    ("marginLeft", "margin-left"),
    ("marginRight", "margin-right"),
    ("marginBottom", "margin-bottom"),
    ("padding", "padding"),
    ("paddingTop", "padding-top"),
    ("paddingLeft", "padding-left"),
    ("paddingRight", "padding-right"),
    ("paddingBottom", "padding-bottom"),
    ("wordBreak", "word-break"),
    ("whiteSpace", "white-space"),
    ("cursor", "cursor"),
    ("listStyleType", "list-style-type"),
];

/// `JAPANESE_TEXT_PATTERN`.
fn is_japanese(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{30ff}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}'
        | '\u{ff01}'..='\u{ff1f}' | '\u{ff21}'..='\u{ff3f}' | '\u{ff41}'..='\u{ff9f}' | '\u{ffe0}'..='\u{ffee}'
        | '\u{20000}'..='\u{2a6df}' | '\u{2a700}'..='\u{2ee5f}' | '\u{2f800}'..='\u{2fa1f}' | '\u{30000}'..='\u{323af}')
}

/// `CHINESE_TEXT_PATTERN`.
fn is_chinese(c: char) -> bool {
    matches!(c, '\u{3100}'..='\u{312f}' | '\u{31a0}'..='\u{31bf}' | '\u{fe10}'..='\u{fe1f}' | '\u{fe50}'..='\u{fe6f}' | '\u{16fe0}'..='\u{16fff}')
}

/// `languageFromText`.
fn language_from_text(text: &str) -> Option<&'static str> {
    if text.chars().any(is_japanese) {
        Some("ja")
    } else if text.chars().any(is_chinese) {
        Some("zh")
    } else {
        None
    }
}

fn is_record(value: &Value) -> bool {
    value.is_object()
}

/// `isSafeStructuredStyleValue`.
fn safe_style_value(value: &str) -> bool {
    !value.contains('\\') && !css::unsafe_function(value) && !css::has_var(value)
}

/// `applyStructuredStyle`.
fn apply_style(element: &mut Element, style: Option<&Value>) {
    let Some(style) = style.and_then(Value::as_object) else { return };
    for (property, css_property) in STYLE_PROPERTIES {
        let value = match style.get(property) {
            Some(Value::Number(number)) if property.starts_with("margin") && property.len() > 6 => {
                format!("{}em", js::number_to_string(number.as_f64().unwrap_or(f64::NAN)))
            }
            Some(Value::Array(items)) if property == "textDecorationLine" => items
                .iter()
                .map(|item| if item.is_null() { String::new() } else { js::to_string(Some(item)) })
                .collect::<Vec<_>>()
                .join(" "),
            Some(Value::String(text)) => text.clone(),
            _ => continue,
        };
        if safe_style_value(&value) {
            element.set_style(css_property, &value);
        }
    }
}

/// `structuredDataAttributeName`.
fn data_attribute_name(key: &str) -> Option<String> {
    let length = js::length(key);
    if length == 0 || length > MAX_DATA_KEY_LENGTH {
        return None;
    }
    let mut chars = key.chars();
    let first = chars.next()?;
    let property = format!("sc{}{}", first.to_uppercase(), chars.as_str());
    let bytes: Vec<char> = property.chars().collect();
    if bytes.windows(2).any(|pair| pair[0] == '-' && pair[1].is_ascii_lowercase()) {
        return None;
    }
    let mut name = String::from("data-");
    for c in property.chars() {
        if c.is_ascii_uppercase() {
            name.push('-');
            name.push(c.to_ascii_lowercase());
        } else {
            name.push(c);
        }
    }
    Some(name)
}

/// Whether `setAttribute` takes this name (an XML `Name`, as far as HTML checks it).
fn valid_attribute_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || "\"'>/=\0".contains(c))
}

/// `applyStructuredData`.
fn apply_data(element: &mut Element, data: Option<&Value>) {
    let Some(data) = data.and_then(Value::as_object) else { return };
    let mut count = 0;
    for (key, raw) in data {
        if count >= MAX_DATA_ATTRIBUTES {
            break;
        }
        if !matches!(raw, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
            continue;
        }
        let value = js::to_string(Some(raw));
        let Some(name) = data_attribute_name(key) else { continue };
        if js::length(&value) > MAX_DATA_VALUE_LENGTH || js::has_control(&value) || !valid_attribute_name(&name) {
            continue;
        }
        element.set_attribute(&name, &value);
        count += 1;
    }
}

/// `normalizeMediaPath`.
fn media_path(value: Option<&Value>) -> Option<&str> {
    let value = value?.as_str()?;
    let length = js::length(value);
    if length == 0 || length > 4096 || value.contains('\\') || js::has_control(value) || value.starts_with('/') {
        return None;
    }
    let mut chars = value.chars();
    if chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        let scheme: String = chars.take_while(|c| c.is_ascii_alphanumeric() || "+.-".contains(*c)).collect();
        if value[1 + scheme.len()..].starts_with(':') {
            return None;
        }
    }
    (!value.split('/').any(|component| component.is_empty() || component == "." || component == "..")).then_some(value)
}

/// `normaliseExternalUrl`.
fn external_url(value: &str) -> Option<String> {
    if js::has_control(value) {
        return None;
    }
    let source = js::trim(value);
    let lower = source.get(..8).unwrap_or(source).to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return None;
    }
    let url = url::Url::parse(source).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.username().is_empty() && url.password().is_none()).then(|| url.to_string())
}

/// `decodeURIComponent`, failing on a malformed escape.
fn decode_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `boundedString(value, maxLength)`: a string's first `max` UTF-16 units.
fn bounded(value: Option<&String>, max: usize) -> String {
    value.map_or_else(String::new, |text| js::slice(text, 0.0, Some(f64::from(u32::try_from(max).unwrap_or(u32::MAX)))))
}

enum Link {
    Internal { query: String, primary_reading: String },
    External(String),
}

/// `parseStructuredLink`.
fn parse_link(href: Option<&Value>) -> Option<Link> {
    let href = href?.as_str()?;
    let length = js::length(href);
    if length == 0 || length > 4096 {
        return None;
    }
    if let Some(query) = href.strip_prefix('?') {
        let mut params: HashMap<String, String> = HashMap::new();
        for part in query.split('&') {
            let (raw_key, raw_value) = part.split_once('=').unwrap_or((part, ""));
            let key = decode_component(&raw_key.replace('+', " "))?;
            let value = decode_component(&raw_value.replace('+', " "))?;
            params.entry(key).or_insert(value);
        }
        let query = js::trim(&bounded(params.get("query"), MAX_LOOKUP_TEXT_BYTES)).to_owned();
        if query.is_empty() {
            return None;
        }
        let primary_reading = js::trim(&bounded(params.get("primary_reading"), MAX_LOOKUP_TEXT_BYTES)).to_owned();
        return Some(Link::Internal { query, primary_reading });
    }
    external_url(href).map(Link::External)
}

/// A path segment of structured content, for limit errors.
#[derive(Clone)]
enum Segment {
    Key(&'static str),
    Index(usize),
}

/// `structuredContentLocation`.
fn location(path: &[Segment]) -> String {
    let omitted = path.len().saturating_sub(MAX_LOCATION_SEGMENTS);
    let mut out = String::new();
    let push = |segment: &Segment, out: &mut String| match segment {
        Segment::Index(index) => {
            out.push('[');
            out.push_str(&index.to_string());
            out.push(']');
        }
        Segment::Key(key) => {
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(key);
        }
    };
    if omitted > 0 {
        for segment in &path[..MAX_LOCATION_SEGMENTS / 2] {
            push(segment, &mut out);
        }
        out.push('[');
        out.push_str(&omitted.to_string());
        out.push_str(" path segments omitted]");
        for segment in &path[path.len() - MAX_LOCATION_SEGMENTS / 2..] {
            push(segment, &mut out);
        }
    } else {
        for segment in path {
            push(segment, &mut out);
        }
    }
    if out.is_empty() { "structuredContent".to_owned() } else { out }
}

/// Where a dictionary image's Anki filename comes from.
pub(crate) type FilenameFor<'a> = dyn FnMut(&str, &str) -> Result<String> + 'a;

/// `state` of `appendTextOnlyGlossary`.
struct State<'a, 'b> {
    nodes: usize,
    dictionary: String,
    /// `appendImage`: `None` leaves images out (plain text).
    images: Option<&'a mut FilenameFor<'b>>,
    path: Vec<Segment>,
}

impl State<'_, '_> {
    /// `countStructuredNode`.
    fn count(&mut self) -> Result<()> {
        if self.nodes >= MAX_STRUCTURED_NODES {
            return Err(Failure::new(format!(
                "Structured content node count {} exceeds limit {MAX_STRUCTURED_NODES} at {}",
                self.nodes + 1,
                location(&self.path)
            )));
        }
        self.nodes += 1;
        Ok(())
    }
}

/// `imageSize` of `anki-glossary.js`.
fn image_size(image: &mut Element, value: &Map<String, Value>) {
    let units = if value.get("sizeUnits").and_then(Value::as_str) == Some("em") { "em" } else { "px" };
    let positive = |size: Option<&Value>| size.and_then(Value::as_f64).filter(|size| size.is_finite() && *size > 0.0);
    let preferred = (positive(value.get("preferredWidth")), positive(value.get("preferredHeight")));
    let own = (positive(value.get("width")), positive(value.get("height")));
    let styled = if preferred.0.is_some() || preferred.1.is_some() {
        preferred
    } else if units == "em" {
        own
    } else {
        (None, None)
    };
    for (dimension, attribute, style) in [("width", own.0, styled.0), ("height", own.1, styled.1)] {
        if units == "px"
            && let Some(size) = attribute
        {
            image.set_attribute(dimension, &js::number_to_string(size));
        }
        if let Some(size) = style {
            image.set_style(dimension, &format!("{}{units}", js::number_to_string(size)));
        }
    }
    let (width, height) = (image.style_value("width"), image.style_value("height"));
    if !width.is_empty() && height.is_empty() {
        image.set_style("height", "auto");
    } else if !height.is_empty() && width.is_empty() {
        image.set_style("width", "auto");
    }
}

/// `appendImage` of `anki-glossary.js`.
fn append_image(parent: &mut Element, value: &Map<String, Value>, path: &str, state: &mut State<'_, '_>) -> Result<()> {
    let Some(filename_for) = state.images.as_deref_mut() else { return Ok(()) };
    let mut image = Element::with_class("img", "gloss-sc-img");
    let filename = filename_for(&state.dictionary, path)?;
    image.set_attribute("alt", value.get("title").and_then(Value::as_str).unwrap_or("Dictionary image"));
    image.set_style("max-width", "100%");
    image.set_style("object-fit", "contain");
    image_size(&mut image, value);
    if !filename.is_empty() {
        image.set_attribute("src", &filename);
    }
    parent.append(image);
    Ok(())
}

/// `appendStructuredValue`, for the Anki layout. `language` is the nearest
/// dictionary-set `lang`.
fn append_value(parent: &mut Element, value: &Value, state: &mut State<'_, '_>, language: Option<&str>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Failure::new(format!("Structured content nesting exceeds limit {MAX_DEPTH} at {}", location(&state.path))));
    }
    state.count()?;
    let child = |parent: &mut Element, value: &Value, segment: Segment, state: &mut State<'_, '_>, language: Option<&str>| {
        state.path.push(segment);
        let result = append_value(parent, value, state, language, depth + 1);
        state.path.pop();
        result
    };
    match value {
        Value::String(text) => {
            parent.append_text(text);
            if language.is_none()
                && let Some(detected) = language_from_text(text)
            {
                parent.set_attribute("lang", detected);
            }
            Ok(())
        }
        Value::Number(_) | Value::Bool(_) => {
            parent.append_text(&js::to_string(Some(value)));
            Ok(())
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                child(parent, item, Segment::Index(index), state, language)?;
            }
            Ok(())
        }
        Value::Null => Ok(()),
        Value::Object(record) => {
            let kind = record.get("type").and_then(Value::as_str);
            if kind == Some("structured-content") {
                return match record.get("content") {
                    Some(content) => child(parent, content, Segment::Key("content"), state, language),
                    None => child(parent, &Value::Null, Segment::Key("content"), state, language).map(drop),
                };
            }
            if kind == Some("text") {
                let property = if record.contains_key("text") { "text" } else { "content" };
                let content = record.get(property).unwrap_or(&Value::Null);
                return child(parent, content, Segment::Key(if property == "text" { "text" } else { "content" }), state, language);
            }
            let tag = if kind == Some("image") {
                "img".to_owned()
            } else {
                record.get("tag").and_then(Value::as_str).map(str::to_lowercase).unwrap_or_default()
            };
            if IGNORED_TAGS.contains(&tag.as_str()) {
                return Ok(());
            }
            if !ALLOWED_TAGS.contains(&tag.as_str()) {
                if let Some(content) = record.get("content") {
                    return child(parent, content, Segment::Key("content"), state, language);
                }
                return Ok(());
            }
            if tag == "img" {
                return match media_path(record.get("path")) {
                    Some(path) => append_image(parent, record, path, state),
                    None => Ok(()),
                };
            }
            append_element(parent, record, &tag, state, language, depth)
        }
    }
}

fn append_element(
    parent: &mut Element,
    record: &Map<String, Value>,
    tag: &str,
    state: &mut State<'_, '_>,
    language: Option<&str>,
    depth: usize,
) -> Result<()> {
    let mut element = Element::new(tag);
    element.add_class(&format!("gloss-sc-{tag}"));
    apply_style(&mut element, record.get("style"));
    apply_data(&mut element, record.get("data"));
    let mut language = language.map(str::to_owned);
    if let Some(lang) = record.get("lang").and_then(Value::as_str)
        && (1..=35).contains(&lang.len())
        && lang.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        element.set_attribute("lang", lang);
        language = Some(lang.to_owned());
    }
    if tag == "td" || tag == "th" {
        for (property, attribute) in [("colSpan", "colspan"), ("rowSpan", "rowspan")] {
            let span = js::to_number(record.get(property));
            if span.fract() == 0.0 && (1.0..=32.0).contains(&span) {
                element.set_attribute(attribute, &js::number_to_string(span));
            }
        }
    }
    if let Some(title) = record.get("title").and_then(Value::as_str)
        && js::length(title) <= 4096
    {
        element.set_attribute("title", title);
    }
    if tag == "details" && record.get("open") == Some(&Value::Bool(true)) {
        element.set_attribute("open", "");
    }
    let mut external = false;
    if tag == "a" {
        element.add_class("gloss-link");
        element.add_class("gsm-hoshidicts-structured-link");
        match parse_link(record.get("href")) {
            Some(Link::Internal { query, primary_reading }) => {
                element.set_attribute("href", "#");
                element.set_data("external", "false");
                element.set_data("hoshidictsQuery", &query);
                if !primary_reading.is_empty() {
                    element.set_data("hoshidictsReading", &primary_reading);
                }
            }
            Some(Link::External(href)) => {
                element.set_attribute("href", &href);
                element.set_attribute("target", "_blank");
                element.set_attribute("rel", "noopener noreferrer");
                element.set_data("external", "true");
                external = true;
            }
            None => {}
        }
    }
    if tag != "br"
        && let Some(content) = record.get("content")
    {
        state.path.push(Segment::Key("content"));
        let result = if tag == "a" {
            let mut text = Element::with_class("span", "gloss-link-text");
            let result = append_value(&mut text, content, state, language.as_deref(), depth + 1);
            element.append(text);
            result
        } else {
            append_value(&mut element, content, state, language.as_deref(), depth + 1)
        };
        state.path.pop();
        result?;
    } else if tag == "a" {
        element.append(Element::with_class("span", "gloss-link-text"));
    }
    if external {
        let mut icon = Element::with_class("span", "gloss-link-external-icon icon");
        icon.set_data("icon", "external-link");
        icon.set_attribute("aria-hidden", "true");
        element.append(icon);
    }
    if tag == "table" {
        let mut container = Element::with_class("div", "gloss-sc-table-container");
        container.append(element);
        parent.append(container);
    } else {
        parent.append(element);
    }
    Ok(())
}

/// `appendTextOnlyGlossary` with `layout: "anki"`.
fn append_glossary(parent: &mut Element, raw: Option<&Value>, state: &mut State<'_, '_>) -> Result<()> {
    let text = raw.and_then(Value::as_str).unwrap_or_default();
    if text.is_empty() {
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
    let items = match parsed {
        Value::Array(items) => items,
        other => vec![other],
    };
    if items.is_empty() {
        return Ok(());
    }
    if items.iter().any(|item| item.get("type").and_then(Value::as_str) == Some("structured-content") && is_record(item)) {
        parent.add_class("structured-content");
    }
    state.path = vec![Segment::Key("glossary"), Segment::Index(0)];
    if let [item] = items.as_slice() {
        return append_value(parent, item, state, None, 0);
    }
    let mut list = Element::with_class("ul", "gloss-list");
    for (index, item) in items.iter().enumerate() {
        state.path = vec![Segment::Key("glossary"), Segment::Index(index)];
        let mut entry = Element::with_class("li", "gloss-item");
        append_value(&mut entry, item, state, None, 0)?;
        list.append(entry);
    }
    parent.append(list);
    Ok(())
}

/// `parseTagList`.
pub(crate) fn tag_list(value: Option<&Value>) -> Vec<String> {
    let text = if js::truthy(value) { js::to_string(value) } else { String::new() };
    text.split(' ').filter(|tag| !tag.is_empty()).map(str::to_owned).collect()
}

/// `definitionTagList`: each tag's name.
pub(crate) fn definition_tags(glossary: &Value) -> Vec<String> {
    match glossary.get("tags") {
        Some(Value::Array(tags)) => tags.iter().map(|tag| js::to_string(tag.get("name"))).collect(),
        _ => tag_list(glossary.get("definitionTags")),
    }
}

// --- Yomitan's class styles (`applyClassStyles`) ---

struct ClassRule {
    selectors: Vec<Selector>,
    css_text: String,
    /// The class names its selectors mention, for `_selectorMightMatch`.
    text: String,
}

fn class_rules() -> &'static [ClassRule] {
    static RULES: OnceLock<Vec<ClassRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let data: Vec<Value> = serde_json::from_str(include_str!("data/structured-content-style.json")).unwrap_or_default();
        data.iter()
            .map(|rule| {
                let selectors: Vec<&str> = rule["selectors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let text = selectors.join(",");
                let css_text: String = rule["styles"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|pair| [pair[0].as_str().unwrap_or_default(), ":", pair[1].as_str().unwrap_or_default(), ";"])
                    .collect();
                // A list `matches` refuses (a pseudo-element) matches nothing.
                let selectors =
                    css::parse_list(&text).filter(|list| list.iter().all(|selector| selector.pseudo.is_none())).unwrap_or_default();
                ClassRule { selectors, css_text, text }
            })
            .collect()
    })
}

/// `rulesForClass`: whether a rule names one of `classes`.
fn might_match(rule: &ClassRule, classes: &[&str]) -> bool {
    classes.iter().any(|token| {
        let needle = format!(".{token}");
        rule.text.match_indices(&needle).any(|(start, _)| {
            !rule.text[start + needle.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    })
}

/// The elements of `root` with a class, by child-index path, and the style
/// `applyClassStyles` writes on each: matching rules first, then its own.
fn class_styles<'a>(element: &'a Element, ancestors: &mut Vec<&'a Element>, path: &mut Vec<usize>, out: &mut Vec<(Vec<usize>, String)>) {
    ancestors.push(element);
    for (index, child) in element.children.iter().enumerate() {
        let Node::Element(child) = child else { continue };
        path.push(index);
        if child.has_attribute("class") {
            let classes = child.classes();
            let mut css_text = String::new();
            for rule in class_rules() {
                if might_match(rule, &classes) && rule.selectors.iter().any(|selector| css::matches(selector, ancestors, child)) {
                    css_text.push_str(&rule.css_text);
                }
            }
            if !css_text.is_empty() {
                css_text.push_str(&child.css_text());
                out.push((path.clone(), css_text));
            }
        }
        class_styles(child, ancestors, path, out);
        path.pop();
    }
    ancestors.pop();
}

fn element_at<'a>(root: &'a mut Element, path: &[usize]) -> Option<&'a mut Element> {
    let mut current = root;
    for index in path {
        current = match current.children.get_mut(*index)? {
            Node::Element(element) => element,
            _ => return None,
        };
    }
    Some(current)
}

/// `applyClassStyles` on `root`'s child `index`: each element's matching class rules
/// become its inline style, ahead of the style it has.
fn apply_class_styles(root: &mut Element, index: usize) {
    let mut styled = Vec::new();
    if let Some(Node::Element(list)) = root.children.get(index) {
        let mut ancestors = vec![&*root];
        class_styles(list, &mut ancestors, &mut Vec::new(), &mut styled);
    }
    let Some(Node::Element(list)) = root.children.get_mut(index) else { return };
    for (path, css_text) in styled {
        if let Some(element) = element_at(list, &path) {
            element.set_attribute("style", &css_text);
        }
    }
}

// --- The definition renderer ---

const BLOCKS: [&str; 13] = ["br", "div", "li", "ol", "p", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "ul"];

/// `plainText`.
fn plain_text(node: &Node, out: &mut String) {
    match node {
        Node::Text(text) => out.push_str(text),
        Node::Raw(_) => {}
        Node::Element(element) => {
            if element.attribute("aria-hidden") == Some("true") {
                return;
            }
            for child in &element.children {
                plain_text(child, out);
            }
            if BLOCKS.contains(&element.name.as_str()) {
                out.push('\n');
            }
        }
    }
}

/// `/\r?\n|\r/u` lines.
fn lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                out.push(&text[start..index]);
                index += if bytes.get(index + 1) == Some(&b'\n') { 2 } else { 1 };
                start = index;
            }
            b'\n' => {
                out.push(&text[start..index]);
                index += 1;
                start = index;
            }
            _ => index += 1,
        }
    }
    out.push(&text[start..]);
    out
}

/// `replaceNewlines`: each line break of the text becomes a `<br>`.
fn replace_newlines(element: &mut Element) {
    let children = std::mem::take(&mut element.children);
    for child in children {
        match child {
            Node::Text(text) => {
                for (index, line) in lines(&text).into_iter().enumerate() {
                    if index > 0 {
                        element.append(Element::new("br"));
                    }
                    element.children.push(Node::text(line));
                }
            }
            Node::Element(mut inner) => {
                replace_newlines(&mut inner);
                element.children.push(Node::Element(inner));
            }
            raw @ Node::Raw(_) => element.children.push(raw),
        }
    }
}

/// What a glossary marker asks for: the extension's renderer options.
#[expect(clippy::struct_excessive_bools, reason = "the extension's renderer options, one flag each")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Wanted {
    pub(crate) dictionary: Option<String>,
    pub(crate) first_only: bool,
    pub(crate) brief: bool,
    pub(crate) no_dictionary: bool,
    pub(crate) plain: bool,
}

/// Reads the dictionaries' styles (`hd_styles`).
pub(crate) type Styles<'a> = dyn FnMut() -> Result<Vec<Value>> + 'a;

/// `createAnkiDefinitionRenderer`, plain and rich at once: `styles` is read once,
/// at the first rich definition.
///
/// A note's rich definitions share one style prefix, where the extension draws
/// one per definition, so each dictionary's stylesheet is scoped once and each
/// set of them parsed once for Smaller Anki cards. Every field on a card sees
/// every field's styles either way, and a dictionary's are the same in each.
pub(crate) struct Renderer<'a> {
    request: &'a Value,
    groups: Vec<(String, Vec<&'a Value>)>,
    aliases: Option<&'a Map<String, Value>>,
    compact: bool,
    styles: Box<Styles<'a>>,
    read_styles: Option<Vec<Value>>,
    filename_for: Box<FilenameFor<'a>>,
    prefix: Option<String>,
    /// Each dictionary's scoped stylesheet, as its `<style>` text.
    scoped: Vec<(String, String)>,
    sheets: compact::Sheets,
}

impl<'a> Renderer<'a> {
    pub(crate) fn new(request: &'a Value, compact: bool, styles: Box<Styles<'a>>, filename_for: Box<FilenameFor<'a>>) -> Self {
        let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
        for glossary in request["term"]["glossaries"].as_array().into_iter().flatten() {
            let dictionary = js::to_string(glossary.get("dictionary"));
            match groups.iter_mut().find(|(name, _)| *name == dictionary) {
                Some((_, list)) => list.push(glossary),
                None => groups.push((dictionary, vec![glossary])),
            }
        }
        Self {
            request,
            groups,
            aliases: request["dictionaryAliases"].as_object(),
            compact,
            styles,
            read_styles: None,
            filename_for,
            prefix: None,
            scoped: Vec::new(),
            sheets: Vec::new(),
        }
    }

    fn alias(&self, dictionary: &str) -> String {
        match self.aliases.and_then(|aliases| aliases.get(dictionary)) {
            Some(alias) => js::to_string(Some(alias)),
            None => dictionary.to_owned(),
        }
    }

    /// `content`: one glossary's body.
    fn content(&mut self, glossary: &Value, images: bool) -> Result<Element> {
        let dictionary = js::to_string(glossary.get("dictionary"));
        let mut body = Element::with_class("div", "gsm-hoshidicts-glossary-content");
        body.set_data("hoshidictsDictionary", &dictionary);
        let mut state = State { nodes: 0, dictionary, images: images.then_some(&mut *self.filename_for), path: Vec::new() };
        append_glossary(&mut body, glossary.get("glossary"), &mut state)?;
        Ok(body)
    }

    fn plain(&mut self, selected: &[(String, Vec<&'a Value>)], no_dictionary: bool) -> Result<String> {
        let mut out: Vec<String> = Vec::new();
        for (dictionary, glossaries) in selected {
            if !no_dictionary {
                out.push(format!("({})", escape_text(&self.alias(dictionary))));
            }
            for glossary in glossaries {
                let mut text = String::new();
                plain_text(&Node::Element(self.content(glossary, false)?), &mut text);
                out.extend(lines(&text).into_iter().map(js::trim).filter(|line| !line.is_empty()).map(escape_text));
            }
        }
        Ok(out.join("<br>"))
    }

    fn entry(&mut self, glossary: &Value, brief: bool, no_dictionary: bool) -> Result<Element> {
        let mut wrapper = Element::new("div");
        let mut labels: Vec<String> = Vec::new();
        if !brief {
            labels.extend(definition_tags(glossary));
            labels.extend(tag_list(glossary.get("termTags")));
            labels.push(if no_dictionary { String::new() } else { self.alias(&js::to_string(glossary.get("dictionary"))) });
            labels.retain(|label| !label.is_empty());
        }
        if !labels.is_empty() {
            let mut meta = Element::with_class("i", "yomitan-glossary-meta");
            meta.set_text(&format!("({})", labels.join(", ")));
            wrapper.append(meta);
            wrapper.append_text(" ");
        }
        let mut body = self.content(glossary, true)?;
        replace_newlines(&mut body);
        wrapper.append(body);
        Ok(wrapper)
    }

    /// `appendStyles`: the selected dictionaries' styles, scoped under each one's items.
    fn append_styles(&mut self, root: &mut Element, selected: &[(String, Vec<&'a Value>)]) -> Result<()> {
        let names: Vec<&str> = selected.iter().map(|(name, _)| name.as_str()).collect();
        let entries: Vec<&Value> = self
            .read_styles
            .iter()
            .flatten()
            .filter(|style| style.get("dictionary").and_then(Value::as_str).is_some_and(|name| names.contains(&name)))
            .collect();
        if entries.is_empty() {
            return Ok(());
        }
        let generation = js::to_string(self.request.get("generation"));
        if self.prefix.is_none() {
            self.prefix = Some(random_prefix()?);
        }
        let prefix = self.prefix.clone().unwrap_or_default();
        let mut seen: Vec<String> = Vec::new();
        let mut total = 0;
        for entry in entries {
            let dictionary = bounded(entry.get("dictionary").and_then(Value::as_str).map(str::to_owned).as_ref(), 4096);
            let styles = bounded(entry.get("styles").and_then(Value::as_str).map(str::to_owned).as_ref(), MAX_DICTIONARY_STYLE_BYTES + 1);
            let bytes = styles.len();
            if dictionary.is_empty()
                || styles.is_empty()
                || seen.contains(&dictionary)
                || bytes > MAX_DICTIONARY_STYLE_BYTES
                || total + bytes > MAX_DICTIONARY_STYLES_BYTES
            {
                continue;
            }
            total += bytes;
            let position = self.scoped.iter().position(|(name, _)| *name == dictionary).unwrap_or_else(|| {
                let scope = format!(".yomitan-glossary [data-dictionary={}]", css::serialize_string(&dictionary));
                self.scoped.push((dictionary.clone(), escape_closing_style(&css::scoped_sheet(&styles, &prefix, &scope))));
                self.scoped.len() - 1
            });
            let text = &self.scoped[position].1;
            let mut style = Element::new("style");
            style.set_data("hoshidictsDictionaryStyle", &dictionary);
            style.set_data("hoshidictsGeneration", &generation);
            style.set_text(text);
            root.append(style);
            seen.push(dictionary);
        }
        Ok(())
    }

    /// `appendDetails`.
    fn append_details(&self, root: &mut Element) {
        let mut details: Vec<String> = Vec::new();
        let term = &self.request["term"];
        if js::truthy(term.get("rules")) {
            details.push(format!("Rules: {}", escape_text(&js::to_string(term.get("rules")))));
        }
        let trace = self.request["trace"].as_array().map(Vec::as_slice).unwrap_or_default();
        if !trace.is_empty() {
            let steps: Vec<String> = trace
                .iter()
                .map(|step| {
                    escape_text(&step.get("name").filter(|name| !name.is_null()).map(|name| js::to_string(Some(name))).unwrap_or_default())
                })
                .collect();
            details.push(format!("Deinflection: {}", steps.join(" &gt; ")));
        }
        if details.is_empty() {
            return;
        }
        let mut small = Element::with_class("small", "yomitan-glossary-details");
        small.append(Node::Raw(details.join("<br>")));
        root.append(small);
    }

    /// One definition marker's value.
    pub(crate) fn render(&mut self, wanted: &Wanted) -> Result<String> {
        let mut selected: Vec<(String, Vec<&'a Value>)> = self
            .groups
            .iter()
            .filter(|(name, _)| wanted.dictionary.as_ref().is_none_or(|dictionary| dictionary == name))
            .cloned()
            .collect();
        if wanted.first_only {
            selected.truncate(1);
        }
        if selected.is_empty() {
            return Ok(String::new());
        }
        if wanted.plain {
            return self.plain(&selected, wanted.no_dictionary);
        }
        if self.read_styles.is_none() {
            self.read_styles = Some((self.styles)()?);
        }
        let mut root = Element::with_class("div", "yomitan-glossary");
        root.set_css_text("text-align: left; contain: layout paint style; isolation: isolate;");
        let mut list = Element::new("ol");
        for (name, glossaries) in &selected {
            for glossary in glossaries {
                let mut page = Element::new("li");
                page.set_data("dictionary", name);
                page.append(self.entry(glossary, wanted.brief, wanted.no_dictionary)?);
                list.append(page);
            }
        }
        root.append(list);
        self.append_styles(&mut root, &selected)?;
        if !wanted.brief && !self.compact {
            self.append_details(&mut root);
        }
        if self.compact {
            return Ok(compact::compact(root, &mut self.sheets));
        }
        apply_class_styles(&mut root, 0);
        Ok(root.outer_html())
    }
}

/// A serialised closing style tag's slash escaped, case kept.
fn escape_closing_style(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower[at..].find("</style") {
        let start = at + found;
        out.push_str(&text[at..start]);
        out.push_str("<\\/");
        out.push_str(&text[start + 2..start + 7]);
        at = start + 7;
    }
    out.push_str(&text[at..]);
    out
}

/// `--hd<32 hex digits>-`: a prefix linked pages cannot guess.
fn random_prefix() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| Failure::new(error.to_string()))?;
    Ok(format!("--hd{}-", crate::host::settings::hex(&bytes)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn render(request: &Value, wanted: &Wanted, compact: bool) -> String {
        let mut renderer = Renderer::new(
            request,
            compact,
            Box::new(|| Ok(vec![])),
            Box::new(|dictionary: &str, path: &str| Ok(format!("{dictionary}/{path}"))),
        );
        renderer.render(wanted).unwrap()
    }

    fn request(glossary: &Value) -> Value {
        json!({
            "generation": 3, "trace": [{ "name": "past" }], "dictionaryAliases": { "J": "Jitendex" },
            "term": { "rules": "v1", "glossaries": [
                { "dictionary": "J", "glossary": glossary.to_string(), "definitionTags": "n vt", "termTags": "P" },
                { "dictionary": "K", "glossary": json!(["二"]).to_string() },
            ] },
        })
    }

    #[test]
    fn renders_plain_and_brief_definitions() {
        let glossary =
            json!(["a < b", { "type": "structured-content", "content": { "tag": "div", "content": ["x", { "tag": "br" }, "y"] } }]);
        let plain = render(&request(&glossary), &Wanted { plain: true, ..Wanted::default() }, false);
        assert_eq!(plain, "(Jitendex)<br>a &lt; b<br>x<br>y<br>(K)<br>二");
        let wanted = Wanted { plain: true, no_dictionary: true, dictionary: Some("K".to_owned()), ..Wanted::default() };
        assert_eq!(render(&request(&glossary), &wanted, false), "二");
        assert_eq!(render(&request(&glossary), &Wanted { dictionary: Some("Z".to_owned()), ..Wanted::default() }, false), "");
    }

    #[test]
    fn checks_paths_names_and_style_tags() {
        let path = vec![Segment::Key("glossary"), Segment::Index(0), Segment::Key("content"), Segment::Index(3)];
        assert_eq!(location(&path), "glossary[0].content[3]");
        assert_eq!(escape_closing_style("a</STYLE>b</style"), "a<\\/STYLE>b<\\/style");
        assert!(media_path(Some(&json!("a/../b"))).is_none());
        assert!(media_path(Some(&json!("http:x"))).is_none());
        assert_eq!(media_path(Some(&json!("img/x.png"))), Some("img/x.png"));
        assert_eq!(data_attribute_name("fooBar"), Some("data-sc-foo-bar".to_owned()));
        assert_eq!(data_attribute_name("a-b"), None);
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
//! The little of the DOM the Anki field renderer uses: elements with ordered
//! attributes, text, an inline style declaration list, and HTML's fragment
//! serialisation (`outerHTML`), as the extension's offscreen document builds and
//! serialises exported glossaries.

/// A node of a rendered field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
    /// Markup the renderer wrote itself (an `innerHTML` of escaped text and `<br>`).
    Raw(String),
}

impl Node {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub(crate) fn as_element(&self) -> Option<&Element> {
        match self {
            Self::Element(element) => Some(element),
            _ => None,
        }
    }

    fn collect_text(&self, out: &mut String) {
        deep(|| match self {
            Self::Text(text) => out.push_str(text),
            Self::Element(element) => element.children.iter().for_each(|child| child.collect_text(out)),
            Self::Raw(_) => {}
        });
    }

    fn serialize(&self, parent: Option<&str>, out: &mut String) {
        deep(|| match self {
            Self::Text(text) if parent.is_some_and(|name| RAW_TEXT.contains(&name)) => out.push_str(text),
            Self::Text(text) => out.push_str(&escape_text(text)),
            Self::Raw(markup) => out.push_str(markup),
            Self::Element(element) => element.serialize(out),
        });
    }
}

impl From<Element> for Node {
    fn from(element: Element) -> Self {
        Self::Element(element)
    }
}

/// Runs one step of a walk over a rendered tree. Structured content has a node
/// limit but no depth limit, so the stack grows onto the heap as a walk goes
/// deeper, where the extension's walks keep their own stacks.
#[inline]
pub(crate) fn deep<R>(step: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(128 * 1024, 1024 * 1024, step)
}

impl Clone for Element {
    // The derived clone follows the nesting.
    fn clone(&self) -> Self {
        deep(|| Self { name: self.name.clone(), attributes: self.attributes.clone(), children: self.children.clone() })
    }
}

impl Drop for Element {
    // The default drop follows the nesting.
    fn drop(&mut self) {
        if self.children.iter().any(|child| matches!(child, Node::Element(_))) {
            deep(|| self.children.clear());
        }
    }
}

const VOID: [&str; 13] = ["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"];
const RAW_TEXT: [&str; 6] = ["style", "script", "xmp", "iframe", "noembed", "noframes"];

/// HTML's text escaping in serialisation.
pub(crate) fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// HTML's attribute-value escaping in serialisation (with `<` and `>`, as current
/// Chromium writes them).
fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// An element.
#[derive(Debug, PartialEq, Eq, Default)]
pub(crate) struct Element {
    pub(crate) name: String,
    pub(crate) attributes: Vec<(String, String)>,
    pub(crate) children: Vec<Node>,
}

impl Element {
    pub(crate) fn new(name: &str) -> Self {
        Self { name: name.to_owned(), attributes: Vec::new(), children: Vec::new() }
    }

    /// An element of `name` with `className`.
    pub(crate) fn with_class(name: &str, class: &str) -> Self {
        let mut element = Self::new(name);
        element.set_attribute("class", class);
        element
    }

    pub(crate) fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub(crate) fn has_attribute(&self, name: &str) -> bool {
        self.attribute(name).is_some()
    }

    /// `setAttribute`: replaces in place, or appends.
    pub(crate) fn set_attribute(&mut self, name: &str, value: &str) {
        match self.attributes.iter_mut().find(|(key, _)| key == name) {
            Some((_, current)) => value.clone_into(current),
            None => self.attributes.push((name.to_owned(), value.to_owned())),
        }
    }

    pub(crate) fn remove_attribute(&mut self, name: &str) {
        self.attributes.retain(|(key, _)| key != name);
    }

    /// The class names (`classList`).
    pub(crate) fn classes(&self) -> Vec<&str> {
        let mut list: Vec<&str> = Vec::new();
        for token in self.attribute("class").unwrap_or_default().split(['\t', '\n', '\x0c', '\r', ' ']) {
            if !token.is_empty() && !list.contains(&token) {
                list.push(token);
            }
        }
        list
    }

    /// `classList.add`: the attribute becomes the ordered set's serialisation.
    pub(crate) fn add_class(&mut self, class: &str) {
        let mut list: Vec<String> = self.classes().into_iter().map(str::to_owned).collect();
        if !list.iter().any(|token| token == class) {
            list.push(class.to_owned());
        }
        self.set_attribute("class", &list.join(" "));
    }

    /// `dataset[key] = value`, `key` in camel case.
    pub(crate) fn set_data(&mut self, key: &str, value: &str) {
        let mut name = String::from("data-");
        for c in key.chars() {
            if c.is_ascii_uppercase() {
                name.push('-');
                name.push(c.to_ascii_lowercase());
            } else {
                name.push(c);
            }
        }
        self.set_attribute(&name, value);
    }

    /// The inline style's declarations, in order.
    pub(crate) fn style(&self) -> Vec<(String, String)> {
        self.attribute("style").map(declarations).unwrap_or_default()
    }

    /// `style.getPropertyValue`.
    pub(crate) fn style_value(&self, property: &str) -> String {
        self.style().into_iter().find(|(name, _)| name == property).map(|(_, value)| value).unwrap_or_default()
    }

    /// `style.setProperty(property, value)`: an empty value removes it.
    pub(crate) fn set_style(&mut self, property: &str, value: &str) {
        let mut list = self.style();
        let value = value.trim();
        if value.is_empty() {
            list.retain(|(name, _)| name != property);
        } else {
            match list.iter_mut().find(|(name, _)| name == property) {
                Some((_, current)) => value.clone_into(current),
                None => list.push((property.to_owned(), value.to_owned())),
            }
        }
        if list.is_empty() && !self.has_attribute("style") {
            return;
        }
        self.set_attribute("style", &css_text(&list));
    }

    /// `style.cssText = text`.
    pub(crate) fn set_css_text(&mut self, text: &str) {
        self.set_attribute("style", &css_text(&declarations(text)));
    }

    /// `style.cssText`.
    pub(crate) fn css_text(&self) -> String {
        css_text(&self.style())
    }

    pub(crate) fn append(&mut self, node: impl Into<Node>) {
        self.children.push(node.into());
    }

    /// `append(text)`: a text node, unless empty.
    pub(crate) fn append_text(&mut self, text: &str) {
        self.children.push(Node::text(text));
    }

    /// `textContent = text`.
    pub(crate) fn set_text(&mut self, text: &str) {
        self.children.clear();
        if !text.is_empty() {
            self.children.push(Node::text(text));
        }
    }

    /// `textContent`.
    pub(crate) fn text_content(&self) -> String {
        let mut text = String::new();
        self.children.iter().for_each(|child| child.collect_text(&mut text));
        text
    }

    /// `outerHTML`.
    pub(crate) fn outer_html(&self) -> String {
        let mut out = String::new();
        self.serialize(&mut out);
        out
    }

    fn serialize(&self, out: &mut String) {
        deep(|| {
            out.push('<');
            out.push_str(&self.name);
            for (name, value) in &self.attributes {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                out.push_str(&escape_attribute(value));
                out.push('"');
            }
            out.push('>');
            if VOID.contains(&self.name.as_str()) {
                return;
            }
            for child in &self.children {
                child.serialize(Some(&self.name), out);
            }
            out.push_str("</");
            out.push_str(&self.name);
            out.push('>');
        });
    }

    /// `normalize()`: adjacent text joined, empty text dropped, throughout.
    pub(crate) fn normalize(&mut self) {
        deep(|| {
            let mut merged: Vec<Node> = Vec::with_capacity(self.children.len());
            for child in std::mem::take(&mut self.children) {
                match (child, merged.last_mut()) {
                    (Node::Text(text), _) if text.is_empty() => {}
                    (Node::Text(text), Some(Node::Text(previous))) => previous.push_str(&text),
                    (Node::Element(mut element), _) => {
                        element.normalize();
                        merged.push(Node::Element(element));
                    }
                    (other, _) => merged.push(other),
                }
            }
            self.children = merged;
        });
    }

    /// Every text node, in document order.
    pub(crate) fn texts_mut(&mut self, visit: &mut impl FnMut(&mut String)) {
        deep(|| {
            for child in &mut self.children {
                match child {
                    Node::Text(text) => visit(text),
                    Node::Element(element) => element.texts_mut(visit),
                    Node::Raw(_) => {}
                }
            }
        });
    }
}

/// A declaration block's declarations, as `name: value` pairs: names lower-cased,
/// later ones replacing earlier ones in place, empty ones dropped.
pub(crate) fn declarations(text: &str) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = Vec::new();
    for declaration in split_top_level(text, ';') {
        let Some(colon) = declaration.find(':') else { continue };
        let name = declaration[..colon].trim();
        let value = collapse(declaration[colon + 1..].trim());
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let name = if name.starts_with("--") { name.to_owned() } else { name.to_ascii_lowercase() };
        match list.iter_mut().find(|(key, _)| *key == name) {
            Some((_, current)) => *current = value,
            None => list.push((name, value)),
        }
    }
    list
}

/// Runs of whitespace outside strings as one space.
fn collapse(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut quote: Option<char> = None;
    let mut space = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == open {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        if c == '"' || c == '\'' {
            quote = Some(c);
        }
        out.push(c);
    }
    out
}

/// `text` split at each `separator` outside strings, brackets and parentheses.
pub(crate) fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (_, '\\') => escaped = true,
            (Some(open), _) if c == open => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth -= 1,
            (None, _) if c == separator && depth == 0 => {
                parts.push(&text[start..index]);
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// `cssText` of declarations.
pub(crate) fn css_text(list: &[(String, String)]) -> String {
    list.iter().map(|(name, value)| format!("{name}: {value};")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_as_html_does() {
        let mut div = Element::with_class("div", "a  b a");
        div.add_class("c");
        div.set_data("hoshidictsDictionary", "J&\"<");
        div.set_style("max-width", "100%");
        div.set_style("object-fit", "contain");
        div.set_style("max-width", "50%");
        div.append_text("x < y & \u{a0}");
        div.append(Element::new("br"));
        let mut style = Element::new("style");
        style.append_text("a > b {}");
        div.append(style);
        assert_eq!(
            div.outer_html(),
            "<div class=\"a b c\" data-hoshidicts-dictionary=\"J&amp;&quot;&lt;\" style=\"max-width: 50%; object-fit: contain;\">\
             x &lt; y &amp; &nbsp;<br><style>a > b {}</style></div>"
        );
        assert_eq!(div.css_text(), "max-width: 50%; object-fit: contain;");
    }

    #[test]
    fn parses_declarations() {
        assert_eq!(
            declarations("Color : red ; content: \"a;  b\"; ; x:; FONT-weight:bold; color: blue"),
            vec![
                ("color".to_owned(), "blue".to_owned()),
                ("content".to_owned(), "\"a;  b\"".to_owned()),
                ("font-weight".to_owned(), "bold".to_owned()),
            ]
        );
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings → Anki → Smaller Anki cards: the extension's `compactAnkiGlossary`
//! (`anki-compact.js`). Each dictionary body of an exported glossary is
//! rewritten as compact HTML from the cascade of the glossary's own dictionary
//! styles; the output rules follow the Compact HTML Cleanup Anki add-on.

use super::css::{self, Computed, Sheet};
use super::dom::{Element, Node};
use crate::host::js;

const SAME: [&str; 13] = ["br", "code", "li", "ol", "rt", "ruby", "sub", "sup", "table", "td", "th", "tr", "ul"];
const UNWRAP: [&str; 3] = ["tbody", "tfoot", "thead"];
const STRUCTURAL: [&str; 8] = ["ol", "ruby", "table", "tbody", "tfoot", "thead", "tr", "ul"];
const BLOCKS: [&str; 6] = ["div", "li", "ol", "p", "table", "ul"];
const CONTAINERS: [&str; 5] = ["div", "li", "p", "td", "th"];
const LISTS: [&str; 3] = ["li", "ol", "ul"];

fn is_inline(display: &str) -> bool {
    display.starts_with("inline") || display.starts_with("ruby") || display.starts_with("contents")
}

fn is_block(node: &Node) -> bool {
    node.as_element().is_some_and(|element| BLOCKS.contains(&element.name.as_str()))
}

fn is_blank(node: &Node) -> bool {
    matches!(node, Node::Text(text) if text.chars().all(|c| c == ' '))
}

fn holds_block(node: &Node) -> bool {
    is_block(node) || node.as_element().is_some_and(|element| element.children.iter().any(holds_block))
}

/// `parseFloat`.
fn parse_float(text: &str) -> f64 {
    let text = js::trim(text);
    let mut end = 0;
    let bytes = text.as_bytes();
    let digits = |from: usize| bytes[from..].iter().take_while(|byte| byte.is_ascii_digit()).count();
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end += 1;
    }
    let whole = digits(end);
    end += whole;
    let mut fraction = 0;
    if bytes.get(end) == Some(&b'.') {
        fraction = digits(end + 1);
        if whole + fraction > 0 {
            end += 1 + fraction;
        }
    }
    if whole + fraction == 0 {
        return f64::NAN;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let exponent = digits(end + 1 + sign);
        if exponent > 0 {
            end += 1 + sign + exponent;
        }
    }
    text[..end].parse().unwrap_or(f64::NAN)
}

/// `text`: line breaks as `<br>`, runs of white space as one space.
fn text_nodes(value: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    for (index, line) in value.split('\n').enumerate() {
        if index > 0 {
            nodes.push(Node::Element(Element::new("br")));
        }
        let mut collapsed = String::new();
        for c in line.chars() {
            if matches!(c, '\t' | '\r' | ' ') {
                if !collapsed.ends_with(' ') {
                    collapsed.push(' ');
                }
            } else {
                collapsed.push(c);
            }
        }
        if !collapsed.is_empty() {
            nodes.push(Node::text(collapsed));
        }
    }
    nodes
}

fn wrap(mut nodes: Vec<Node>, tags: &[&'static str]) -> Vec<Node> {
    if nodes.is_empty() {
        return nodes;
    }
    for tag in tags {
        let mut element = Element::new(tag);
        element.children = nodes;
        nodes = vec![Node::Element(element)];
    }
    nodes
}

/// `emphasis`: the tags a structured-content style asks for.
fn emphasis(name: &str, element: &Element) -> Vec<&'static str> {
    let weight = element.style_value("font-weight");
    let decoration = format!("{} {}", element.style_value("text-decoration-line"), element.style_value("text-decoration"));
    let mut tags = Vec::new();
    if name == "strong" || weight == "bold" || weight == "bolder" || js::string_to_number(&weight) >= 600.0 {
        tags.push("b");
    }
    if name == "em" || element.style_value("font-style") == "italic" {
        tags.push("i");
    }
    if decoration.contains("underline") {
        tags.push("u");
    }
    if decoration.contains("line-through") {
        tags.push("s");
    }
    tags
}

/// `defaultMarker`.
fn default_marker(ancestors: &[&Element], list: &Element) -> &'static str {
    if list.name == "ol" {
        return "decimal";
    }
    let depth = ancestors.iter().rev().filter(|parent| parent.name == "ul" || parent.name == "ol").count().min(2);
    ["disc", "circle", "square"][depth]
}

/// `keptAttributes`.
fn kept_attributes(ancestors: &[&Element], element: &Element) -> Vec<(String, String)> {
    let mut kept = Vec::new();
    for name in ["rowspan", "colspan"] {
        if let Some(value) = element.attribute(name) {
            kept.push((name.to_owned(), value.to_owned()));
        }
    }
    if let Some(lang) = element.attribute("lang") {
        let inherited = ancestors.iter().rev().find_map(|parent| parent.attribute("lang"));
        if inherited != Some(lang) {
            kept.push(("lang".to_owned(), lang.to_owned()));
        }
    }
    if let Some(content) = element.attribute("data-sc-content") {
        kept.push(("data-sc-content".to_owned(), content.to_owned()));
    }
    kept
}

/// `outputTag`.
fn output_tag(name: &str, display: &str, kept: &[(String, String)]) -> Option<String> {
    if SAME.contains(&name) {
        return Some(name.to_owned());
    }
    if UNWRAP.contains(&name) {
        return (!kept.is_empty()).then(|| name.to_owned());
    }
    if is_inline(display) {
        return (!kept.is_empty()).then(|| "span".to_owned());
    }
    Some(if name == "p" { "p" } else { "div" }.to_owned())
}

/// `flatten`.
fn flatten(mut tag: Option<String>, mut nodes: Vec<Node>, kept: &[(String, String)]) -> (Option<String>, Vec<Node>) {
    if tag.as_deref().is_some_and(|tag| CONTAINERS.contains(&tag))
        && let [Node::Element(only)] = nodes.as_slice()
        && only.name == "div"
        && only.attributes.is_empty()
    {
        let Some(Node::Element(only)) = nodes.pop() else { unreachable!() };
        nodes = only.children;
    }
    if tag.as_deref() == Some("p") && nodes.iter().any(holds_block) {
        tag = Some("div".to_owned());
    }
    if matches!(tag.as_deref(), Some("div" | "p")) && kept.is_empty() && nodes.iter().all(|node| is_block(node) || is_blank(node)) {
        tag = None;
    }
    (tag, nodes)
}

struct Compactor<'s> {
    sheet: &'s Sheet,
}

impl Compactor<'_> {
    fn pseudo(&self, ancestors: &[&Element], element: &Element, style: &Computed, which: &str) -> Vec<Node> {
        let computed = self.sheet.computed(ancestors, element, style, Some(which));
        if computed.display != "none" && computed.visibility == "visible" {
            text_nodes(&css::generated_text(&computed.content))
        } else {
            Vec::new()
        }
    }

    /// `compact`: `styled` is the node in the tree the cascade reads, `original`
    /// the same node as exported.
    fn compact<'a>(
        &self,
        styled: &'a Node,
        original: &Node,
        ancestors: &mut Vec<&'a Element>,
        parent: &Computed,
        applied: &[&'static str],
        pending: &[&'static str],
    ) -> Vec<Node> {
        let (Node::Element(element), Node::Element(source)) = (styled, original) else {
            return match styled {
                Node::Text(text) => wrap(text_nodes(text), pending),
                _ => Vec::new(),
            };
        };
        if element.name == "rp" || element.name == "style" {
            return Vec::new();
        }
        let style = self.sheet.computed(ancestors, element, parent, None);
        if style.display == "none" || style.visibility != "visible" {
            return Vec::new();
        }
        if element.name == "img" {
            return vec![Node::Element(compact_image(element, source, &style))];
        }
        let mut nodes = self.compact_element(element, source, ancestors, &style, parent, applied, pending);
        if style.display.starts_with("inline") {
            if parse_float(&style.margin_left) > 0.0 {
                nodes.insert(0, Node::text(" "));
            }
            if parse_float(&style.margin_right) > 0.0 {
                nodes.push(Node::text(" "));
            }
        }
        nodes
    }

    #[expect(clippy::too_many_arguments, reason = "the extension's compactElement, with the tree walk's state")]
    fn compact_element<'a>(
        &self,
        element: &'a Element,
        source: &Element,
        ancestors: &mut Vec<&'a Element>,
        style: &Computed,
        parent: &Computed,
        applied: &[&'static str],
        pending: &[&'static str],
    ) -> Vec<Node> {
        let name = element.name.as_str();
        let kept = kept_attributes(ancestors, element);
        let initial = output_tag(name, &style.display, &kept);
        let mut wanted: Vec<&'static str> = Vec::new();
        for tag in pending.iter().copied().chain(emphasis(name, element)) {
            if !wanted.contains(&tag) && !applied.contains(&tag) {
                wanted.push(tag);
            }
        }
        let structural = STRUCTURAL.contains(&initial.as_deref().unwrap_or(name));
        let before = self.pseudo(ancestors, element, style, "before");
        let after = self.pseudo(ancestors, element, style, "after");
        let mut nodes = before;
        ancestors.push(element);
        let deeper: Vec<&'static str> = applied.iter().chain(&wanted).copied().collect();
        for (child, original) in element.children.iter().zip(&source.children) {
            nodes.extend(if structural {
                self.compact(child, original, ancestors, style, applied, &wanted)
            } else {
                self.compact(child, original, ancestors, style, &deeper, &[])
            });
        }
        ancestors.pop();
        nodes.extend(after);
        let (tag, nodes) = flatten(initial, nodes, &kept);
        let nodes = if structural { nodes } else { wrap(nodes, &wanted) };
        let Some(tag) = tag else { return nodes };
        let mut output = Element::new(&tag);
        for (key, value) in &kept {
            output.set_attribute(key, value);
        }
        if LISTS.contains(&tag.as_str()) {
            let inherited = if tag == "li" { parent.list_style_type.as_str() } else { default_marker(ancestors, element) };
            if style.list_style_type != inherited {
                output.set_style("list-style-type", &style.list_style_type);
            }
        }
        output.children = nodes;
        vec![Node::Element(output)]
    }

    /// Each dictionary body below `element`, by child-index path, compacted.
    fn bodies<'a>(
        &self,
        element: &'a Element,
        source: &Element,
        ancestors: &mut Vec<&'a Element>,
        style: &Computed,
        path: &mut Vec<usize>,
        out: &mut Vec<(Vec<usize>, Element)>,
    ) {
        ancestors.push(element);
        for (index, (child, original)) in element.children.iter().zip(&source.children).enumerate() {
            let (Node::Element(child), Node::Element(original)) = (child, original) else { continue };
            path.push(index);
            let computed = self.sheet.computed(ancestors, child, style, None);
            if child.classes().contains(&"gsm-hoshidicts-glossary-content") {
                let mut replacement = Element::new("div");
                ancestors.push(child);
                for (node, source) in child.children.iter().zip(&original.children) {
                    replacement.children.extend(self.compact(node, source, ancestors, &computed, &[], &[]));
                }
                ancestors.pop();
                replacement.normalize();
                replacement.texts_mut(&mut |text| {
                    while text.contains("  ") {
                        *text = text.replace("  ", " ");
                    }
                });
                out.push((path.clone(), replacement));
            } else {
                self.bodies(child, original, ancestors, &computed, path, out);
            }
            path.pop();
        }
        ancestors.pop();
    }
}

/// `compactImage`.
fn compact_image(styled: &Element, source: &Element, style: &Computed) -> Element {
    let mut copy = Element::new("img");
    if let Some(src) = source.attribute("src") {
        copy.set_attribute("src", src);
    }
    for name in ["alt", "width", "height"] {
        if let Some(value) = styled.attribute(name) {
            copy.set_attribute(name, value);
        }
    }
    for property in ["width", "height", "max-width"] {
        let value = styled.style_value(property);
        if !value.is_empty() {
            copy.set_style(property, &value);
        }
    }
    let align = &style.vertical_align;
    if !align.is_empty() && align.chars().all(|c| c.is_ascii_lowercase() || c == '-') && align != "baseline" {
        copy.set_style("vertical-align", align);
    }
    copy
}

fn strip_sources(element: &mut Element) {
    if element.name == "img" {
        element.remove_attribute("src");
    }
    for child in &mut element.children {
        if let Node::Element(child) = child {
            strip_sources(child);
        }
    }
}

fn style_text(element: &Element, out: &mut String) {
    for child in &element.children {
        if let Node::Element(child) = child {
            if child.name == "style" {
                out.push_str(&child.text_content());
                out.push('\n');
            } else {
                style_text(child, out);
            }
        }
    }
}

fn remove_styles(element: &mut Element) {
    element.children.retain(|child| !matches!(child, Node::Element(child) if child.name == "style"));
    for child in &mut element.children {
        if let Node::Element(child) = child {
            remove_styles(child);
        }
    }
}

fn replace_at(root: &mut Element, path: &[usize], replacement: Element) {
    let Some((last, parents)) = path.split_last() else { return };
    let mut current = root;
    for index in parents {
        match current.children.get_mut(*index) {
            Some(Node::Element(element)) => current = element,
            _ => return,
        }
    }
    if let Some(slot) = current.children.get_mut(*last) {
        *slot = Node::Element(replacement);
    }
}

/// Parsed sheets by their text, for the definitions of one note.
pub(crate) type Sheets = Vec<(String, Sheet)>;

/// `compactAnkiGlossary(document, root)`, with the cascade of `root`'s own styles,
/// each text parsed once in `sheets`.
pub(crate) fn compact(mut root: Element, sheets: &mut Sheets) -> String {
    let mut text = String::new();
    style_text(&root, &mut text);
    let position = sheets.iter().position(|(own, _)| *own == text).unwrap_or_else(|| {
        let sheet = Sheet::new(&text);
        sheets.push((text, sheet));
        sheets.len() - 1
    });
    let compactor = Compactor { sheet: &sheets[position].1 };
    let mut styled = root.clone();
    strip_sources(&mut styled);
    let mut replacements = Vec::new();
    let style = compactor.sheet.computed(&[], &styled, &Computed::default(), None);
    compactor.bodies(&styled, &root, &mut Vec::new(), &style, &mut Vec::new(), &mut replacements);
    for (path, replacement) in replacements {
        replace_at(&mut root, &path, replacement);
    }
    remove_styles(&mut root);
    root.set_css_text("text-align: left;");
    root.outer_html()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(children: Vec<Node>) -> Element {
        let mut root = Element::with_class("div", "yomitan-glossary");
        let mut page = Element::new("li");
        page.set_data("dictionary", "J");
        let mut content = Element::with_class("div", "gsm-hoshidicts-glossary-content");
        content.children = children;
        page.append(content);
        let mut list = Element::new("ol");
        list.append(page);
        root.append(list);
        root
    }

    #[test]
    fn compacts_bodies_from_their_dictionary_styles() {
        let mut bold = Element::with_class("span", "gloss-sc-span");
        bold.set_style("font-weight", "bold");
        bold.set_style("margin-left", "0.5em");
        bold.append_text("b  c");
        let mut hidden = Element::with_class("span", "hide");
        hidden.append_text("gone");
        let mut wrapper = Element::new("div");
        wrapper.append(Element::with_class("div", "x"));
        let mut tagged = Element::new("span");
        tagged.set_attribute("data-sc-content", "note");
        tagged.append_text("n");
        let mut root = body(vec![Node::text("a\tz"), bold.into(), hidden.into(), tagged.into()]);
        let mut style = Element::new("style");
        style.set_text(
            ".yomitan-glossary .hide { display: none } .gloss-sc-span::before { content: \"[\" } .gloss-sc-span::after { content: '\\5D' }",
        );
        root.append(style);
        assert_eq!(
            compact(root, &mut Vec::new()),
            "<div class=\"yomitan-glossary\" style=\"text-align: left;\"><ol><li data-dictionary=\"J\"><div>a z <b>[b c]</b>\
             <span data-sc-content=\"note\">n</span></div></li></ol></div>"
        );
    }

    #[test]
    fn parses_floats_as_javascript_does() {
        assert!((parse_float(" 1.5em") - 1.5).abs() < f64::EPSILON);
        assert!(parse_float("auto").is_nan());
        assert!((parse_float("-.5e1px") + 5.0).abs() < f64::EPSILON);
    }
}

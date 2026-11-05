// SPDX-License-Identifier: GPL-3.0-or-later
//! The CSS the Anki field renderer needs without a browser: dictionary
//! stylesheets parsed, filtered, renamed and scoped as `applyDictionaryStyles`
//! does through the CSSOM, selectors matched against a rendered field, and the
//! small cascade Smaller Anki cards reads computed styles from.
//!
//! The browser serialises what it parsed in its own canonical form; this keeps
//! selectors and values as the dictionary wrote them, with whitespace collapsed.

use std::collections::HashMap;

use super::dom::{self, Element, Node};

/// A declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Declaration {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) important: bool,
}

/// A rule of a stylesheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rule {
    /// A style rule, with the rules nested in it.
    Style { selector: String, declarations: Vec<Declaration>, rules: Vec<Self> },
    /// `@media`, `@supports` or `@container`, by name, with its prelude.
    Group { name: String, prelude: String, rules: Vec<Self> },
    /// Any other at-rule.
    Other,
}

const GROUPS: [&str; 3] = ["media", "supports", "container"];

/// Strips comments, outside strings.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == open || c == '\n' {
                quote = None;
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for next in chars.by_ref() {
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
            out.push(' ');
        } else {
            if c == '"' || c == '\'' {
                quote = Some(c);
            } else if c == '\\' {
                out.push(c);
                out.extend(chars.next());
                continue;
            }
            out.push(c);
        }
    }
    out
}

/// The end of the block that starts after `open` (the index past its `}`).
fn block_end(text: &str, open: usize) -> usize {
    let mut depth = 1_i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, c) in text[open..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (_, '\\') => escaped = true,
            (Some(q), _) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => {
                depth -= 1;
                if depth == 0 {
                    return open + index + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// The next `{` or `;` at the top level of `text` from `start`.
fn next_stop(text: &str, start: usize) -> Option<(usize, char)> {
    let mut depth = 0_i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, c) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (_, '\\') => escaped = true,
            (Some(q), _) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, '{' | ';' | '}') if depth <= 0 => return Some((start + index, c)),
            _ => {}
        }
    }
    None
}

/// `CSSStyleSheet.replaceSync`: the rules of `text`.
pub(crate) fn parse_sheet(text: &str) -> Vec<Rule> {
    let text = strip_comments(text);
    rules(&text)
}

fn rules(text: &str) -> Vec<Rule> {
    let mut list = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let Some((stop, c)) = next_stop(text, at) else { break };
        let prelude = text[at..stop].trim();
        match c {
            ';' | '}' => {
                // A statement at-rule (@import, @charset, ...) or stray text.
                if prelude.starts_with('@') {
                    list.push(Rule::Other);
                }
                at = stop + 1;
            }
            _ => {
                let end = block_end(text, stop + 1);
                let body = &text[stop + 1..end.saturating_sub(1).max(stop + 1)];
                list.push(rule(prelude, body));
                at = end;
            }
        }
    }
    list
}

fn rule(prelude: &str, body: &str) -> Rule {
    if let Some(at_rule) = prelude.strip_prefix('@') {
        let name_end = at_rule.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(at_rule.len());
        let name = at_rule[..name_end].to_ascii_lowercase();
        if GROUPS.contains(&name.as_str()) {
            return Rule::Group { name, prelude: collapse(at_rule[name_end..].trim()), rules: rules(body) };
        }
        return Rule::Other;
    }
    let (declarations, rules) = style_body(body);
    Rule::Style { selector: collapse(prelude), declarations, rules }
}

/// A style rule's body: its declarations and nested rules.
fn style_body(body: &str) -> (Vec<Declaration>, Vec<Rule>) {
    let mut declarations: Vec<Declaration> = Vec::new();
    let mut nested = Vec::new();
    let mut at = 0;
    while at < body.len() {
        match next_stop(body, at) {
            Some((stop, '{')) => {
                let end = block_end(body, stop + 1);
                let prelude = body[at..stop].trim();
                nested.push(rule(prelude, &body[stop + 1..end.saturating_sub(1).max(stop + 1)]));
                at = end;
            }
            Some((stop, _)) => {
                push_declaration(&mut declarations, &body[at..stop]);
                at = stop + 1;
            }
            None => {
                push_declaration(&mut declarations, &body[at..]);
                break;
            }
        }
    }
    (declarations, nested)
}

fn push_declaration(list: &mut Vec<Declaration>, text: &str) {
    let Some(colon) = text.find(':') else { return };
    let name = text[..colon].trim();
    let mut value = text[colon + 1..].trim().to_owned();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return;
    }
    let mut important = false;
    let lower = value.to_ascii_lowercase();
    if let Some(bang) = lower.rfind('!')
        && lower[bang + 1..].trim() == "important"
    {
        important = true;
        value = value[..bang].trim().to_owned();
    }
    let value = collapse(&value);
    if value.is_empty() && !name.starts_with("--") {
        return;
    }
    let name = if name.starts_with("--") { name.to_owned() } else { name.to_ascii_lowercase() };
    list.retain(|declaration| declaration.name != name);
    list.push(Declaration { name, value, important });
}

/// Whitespace runs outside strings as one space.
fn collapse(text: &str) -> String {
    let list = dom::declarations(&format!("x:{text}"));
    list.into_iter().next().map(|(_, value)| value).unwrap_or_default()
}

/// A declaration block's `cssText`.
pub(crate) fn declarations_text(list: &[Declaration]) -> String {
    list.iter()
        .map(|declaration| {
            format!("{}: {}{};", declaration.name, declaration.value, if declaration.important { " !important" } else { "" })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
}

/// A rule's `cssText`.
pub(crate) fn rule_text(rule: &Rule) -> String {
    match rule {
        Rule::Style { selector, declarations, rules } if rules.is_empty() => {
            let block = declarations_text(declarations);
            if block.is_empty() { format!("{selector} {{ }}") } else { format!("{selector} {{ {block} }}") }
        }
        Rule::Style { selector, declarations, rules } => {
            let mut lines: Vec<String> = Vec::new();
            if !declarations.is_empty() {
                lines.push(indent(&declarations_text(declarations)));
            }
            lines.extend(rules.iter().map(|rule| indent(&rule_text(rule))));
            format!("{selector} {{\n{}\n}}", lines.join("\n"))
        }
        Rule::Group { name, prelude, rules } => {
            let inner: Vec<String> = rules.iter().map(|rule| indent(&rule_text(rule))).collect();
            let head = if prelude.is_empty() { format!("@{name}") } else { format!("@{name} {prelude}") };
            if inner.is_empty() { format!("{head} {{\n}}") } else { format!("{head} {{\n{}\n}}", inner.join("\n")) }
        }
        Rule::Other => String::new(),
    }
}

// --- Dictionary stylesheets (`applyDictionaryStyles`) ---

const VARIABLES: [&str; 5] = ["--text-color", "--background-color", "--fg", "--canvas", "--font-size-no-units"];
const FONT_FAMILIES: [&str; 17] = [
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-serif",
    "ui-sans-serif",
    "ui-monospace",
    "ui-rounded",
    "math",
    "fangsong",
    "inherit",
    "initial",
    "unset",
    "revert",
    "revert-layer",
];

/// `\w`, with `u`: ASCII letters, digits and `_`.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `[\w\P{ASCII}-]`.
fn is_name(c: char) -> bool {
    is_word(c) || !c.is_ascii() || c == '-'
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text.is_char_boundary(prefix.len()) && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// The length of a `--name` run at the start of `text`, if `text` starts with one.
fn custom_name(text: &str) -> Option<usize> {
    let rest = text.strip_prefix("--")?;
    let run: usize = rest.chars().take_while(|c| is_name(*c)).map(char::len_utf8).sum();
    (run > 0).then_some(2 + run)
}

/// `UNSAFE_STYLE_FUNCTION`: `url()`, `src()`, `image-set()`, `paint()`, `attr()` or a custom function.
pub(crate) fn unsafe_function(text: &str) -> bool {
    let mut previous: Option<char> = None;
    for (index, c) in text.char_indices() {
        let rest = &text[index..];
        if !previous.is_some_and(is_word) {
            for name in ["url", "src", "image-set", "paint", "attr"] {
                if starts_with_ignore_case(rest, name) && rest[name.len()..].trim_start_matches(crate::host::js::is_space).starts_with('(')
                {
                    return true;
                }
            }
        }
        if !previous.is_some_and(is_name)
            && let Some(length) = custom_name(rest)
            && rest[length..].starts_with('(')
        {
            return true;
        }
        previous = Some(c);
    }
    false
}

/// `/\bvar\s*\(/iu`.
pub(crate) fn has_var(text: &str) -> bool {
    let mut previous: Option<char> = None;
    for (index, c) in text.char_indices() {
        let rest = &text[index..];
        if !previous.is_some_and(is_word)
            && starts_with_ignore_case(rest, "var")
            && rest[3..].trim_start_matches(crate::host::js::is_space).starts_with('(')
        {
            return true;
        }
        previous = Some(c);
    }
    false
}

/// The families of a `font` shorthand: what follows its size (and line height).
fn shorthand_families(value: &str) -> Option<String> {
    let words = dom::split_top_level(value, ' ');
    let sized = words.iter().position(|word| {
        let word = word.split('/').next().unwrap_or_default().to_ascii_lowercase();
        word.starts_with(|c: char| c.is_ascii_digit() || c == '.')
            || ["xx-small", "x-small", "small", "medium", "large", "x-large", "xx-large", "xxx-large", "smaller", "larger"]
                .contains(&word.as_str())
    })?;
    Some(words[sized + 1..].join(" "))
}

/// `isSafeDictionaryStyle`.
fn safe_style(declarations: &[Declaration]) -> bool {
    let text = declarations_text(declarations);
    if text.contains('\\') || unsafe_function(&text) {
        return false;
    }
    let allowed = |families: &str| {
        dom::split_top_level(families, ',').iter().all(|family| FONT_FAMILIES.contains(&family.trim().to_ascii_lowercase().as_str()))
    };
    declarations.iter().all(|declaration| match declaration.name.as_str() {
        "font-family" => allowed(&declaration.value),
        "font" if !has_var(&declaration.value) => shorthand_families(&declaration.value).is_none_or(|families| allowed(&families)),
        _ => true,
    })
}

/// `typeDictionaryStyleVariables`: the dictionary's own custom properties renamed
/// under `prefix`, and the shared aliases typed at each `var()`.
fn type_variables(declarations: &mut Vec<Declaration>, prefix: &str) {
    let text = declarations_text(declarations);
    if !text.contains("--") {
        return;
    }
    let mut out = String::with_capacity(text.len());
    let mut suffixes: Vec<&str> = Vec::new();
    let mut previous: Option<char> = None;
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let c = rest.chars().next().unwrap_or_default();
        // Strings and comments stay opaque.
        let quoted = (c == '"' || c == '\'').then(|| rest[1..].find(c).map(|end| end + 2)).flatten();
        let comment = rest.starts_with("/*").then(|| rest[2..].find("*/").map(|end| end + 4)).flatten();
        if let Some(length) = quoted.or(comment) {
            out.push_str(&rest[..length]);
            previous = rest[..length].chars().last();
            at += length;
            continue;
        }
        if !previous.is_some_and(is_word) && starts_with_ignore_case(rest, "var(") {
            let spaces: usize = rest[4..].chars().take_while(|c| crate::host::js::is_space(*c)).map(char::len_utf8).sum();
            if let Some(length) = custom_name(&rest[4 + spaces..]) {
                let token = &rest[..4 + spaces + length];
                let name = &rest[4 + spaces..4 + spaces + length];
                if VARIABLES.contains(&name) {
                    let numeric = name == "--font-size-no-units";
                    suffixes.push(if numeric { " * 1)" } else { " 100%, transparent)" });
                    out.push_str(if numeric { "calc(" } else { "color-mix(in srgb, " });
                    out.push_str(token);
                } else {
                    suffixes.push("");
                    out.push_str(&token.replacen(name, &format!("{prefix}{}", &name[2..]), 1));
                }
                previous = token.chars().last();
                at += token.len();
                continue;
            }
        }
        if !previous.is_some_and(is_name)
            && let Some(length) = custom_name(rest)
        {
            let name = &rest[..length];
            if VARIABLES.contains(&name) {
                out.push_str(name);
            } else {
                out.push_str(prefix);
                out.push_str(&name[2..]);
            }
            previous = name.chars().last();
            at += length;
            continue;
        }
        match c {
            '(' => suffixes.push(""),
            ')' => {
                out.push(')');
                out.push_str(suffixes.pop().unwrap_or_default());
                previous = Some(')');
                at += 1;
                continue;
            }
            _ => {}
        }
        out.push(c);
        previous = Some(c);
        at += c.len_utf8();
    }
    let (reparsed, _) = style_body(&out);
    *declarations = reparsed;
}

/// `filterDictionaryStyleRules`.
fn filter(rules: &mut Vec<Rule>, prefix: &str) {
    rules.retain_mut(|rule| match rule {
        Rule::Style { declarations, rules, .. } => {
            if !safe_style(declarations) {
                return false;
            }
            type_variables(declarations, prefix);
            filter(rules, prefix);
            true
        }
        Rule::Group { rules, .. } => {
            filter(rules, prefix);
            true
        }
        Rule::Other => false,
    });
}

/// `splitSelectorList`: a selector list's top-level members.
pub(crate) fn split_selector_list(text: &str) -> Vec<String> {
    let mut selectors = Vec::new();
    let (mut depth, mut quote, mut start) = (0_i32, None::<char>, 0);
    let mut chars = text.char_indices();
    while let Some((index, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if let Some(open) = quote {
            if c == open {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '(' || c == '[' {
            depth += 1;
        } else if c == ')' || c == ']' {
            depth -= 1;
        } else if c == ',' && depth == 0 {
            selectors.push(text[start..index].trim().to_owned());
            start = index + 1;
        }
    }
    selectors.push(text[start..].trim().to_owned());
    selectors
}

/// `prefixDictionaryStyleRules`.
fn prefix_rules(rules: &mut [Rule], scope: &str) {
    for rule in rules {
        match rule {
            Rule::Style { selector, .. } => {
                *selector = split_selector_list(selector).iter().map(|member| format!("{scope} {member}")).collect::<Vec<_>>().join(", ");
            }
            Rule::Group { rules, .. } => prefix_rules(rules, scope),
            Rule::Other => {}
        }
    }
}

/// CSSOM's serialisation of a string.
pub(crate) fn serialize_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\0' => out.push('\u{fffd}'),
            '\u{1}'..='\u{1f}' | '\u{7f}' => {
                let escaped = format!("\\{:x} ", u32::from(c));
                out.push_str(&escaped);
            }
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One dictionary's stylesheet, as `applyDictionaryStyles` with `scope` leaves it:
/// the text of its `<style>`, or `None` when nothing survives the parse.
pub(crate) fn scoped_sheet(styles: &str, prefix: &str, scope: &str) -> String {
    let mut sheet = parse_sheet(styles);
    filter(&mut sheet, prefix);
    prefix_rules(&mut sheet, scope);
    sheet.iter().map(rule_text).collect::<Vec<_>>().join("\n")
}

// --- Selectors ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    Descendant,
    Child,
    Next,
    Subsequent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Simple {
    Type(String),
    Id(String),
    Class(String),
    Attribute {
        name: String,
        operator: Option<char>,
        value: String,
        insensitive: bool,
    },
    /// `:not`, `:is` or `:where`.
    Logical {
        negated: bool,
        list: Vec<Selector>,
    },
    /// `:nth-child` and its kin: (a, b, from the end, of type).
    Nth(i64, i64, bool, bool),
    Empty,
    /// A pseudo-class that never matches a rendered field (`:hover`, `:root`, ...).
    Never,
}

/// A complex selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selector {
    /// Compounds from left to right, each with the combinator before it.
    compounds: Vec<(Combinator, Vec<Simple>)>,
    /// `::before`, `::after`, or another pseudo-element.
    pub(crate) pseudo: Option<String>,
    pub(crate) specificity: (u32, u32, u32),
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn spaces(&mut self) -> bool {
        let start = self.at;
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
        self.at > start
    }

    fn identifier(&mut self) -> Option<String> {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c == '\\' {
                self.bump();
                let hex: String = self.text[self.at..].chars().take_while(char::is_ascii_hexdigit).take(6).collect();
                if hex.is_empty() {
                    out.extend(self.bump());
                } else {
                    self.at += hex.len();
                    out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
                    if self.peek().is_some_and(char::is_whitespace) {
                        self.bump();
                    }
                }
            } else if is_name(c) {
                out.push(c);
                self.bump();
            } else {
                break;
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn string(&mut self, quote: char) -> Option<String> {
        let mut out = String::new();
        loop {
            match self.bump()? {
                '\\' => out.extend(self.bump()),
                c if c == quote => return Some(out),
                c => out.push(c),
            }
        }
    }

    /// The text up to the `)` that closes the current parenthesis.
    fn argument(&mut self) -> Option<&str> {
        let start = self.at;
        let mut depth = 1_i32;
        let mut quote: Option<char> = None;
        while let Some(c) = self.bump() {
            match (quote, c) {
                (_, '\\') => {
                    self.bump();
                }
                (Some(q), _) if q == c => quote = None,
                (None, '"' | '\'') => quote = Some(c),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&self.text[start..self.at - 1]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn attribute(&mut self) -> Option<Simple> {
        self.spaces();
        let name = self.identifier()?.to_ascii_lowercase();
        self.spaces();
        let operator = match self.bump()? {
            ']' => return Some(Simple::Attribute { name, operator: None, value: String::new(), insensitive: false }),
            '=' => '=',
            c @ ('~' | '|' | '^' | '$' | '*') if self.bump() == Some('=') => c,
            _ => return None,
        };
        self.spaces();
        let value = match self.peek()? {
            q @ ('"' | '\'') => {
                self.bump();
                self.string(q)?
            }
            _ => self.identifier()?,
        };
        self.spaces();
        let mut insensitive = false;
        if let Some(flag) = self.peek().filter(char::is_ascii_alphabetic) {
            self.bump();
            insensitive = flag.eq_ignore_ascii_case(&'i');
        }
        self.spaces();
        (self.bump()? == ']').then_some(Simple::Attribute { name, operator: Some(operator), value, insensitive })
    }

    fn pseudo_class(&mut self, name: &str) -> Option<(Simple, (u32, u32, u32))> {
        let one = (0, 1, 0);
        let functional = self.peek() == Some('(');
        if functional {
            self.bump();
            let argument = self.argument()?.to_owned();
            return match name {
                "not" | "is" | "where" | "matches" | "-webkit-any" => {
                    let list = parse_list(&argument)?;
                    if list.iter().any(|selector| selector.pseudo.is_some()) {
                        return None;
                    }
                    let most = list.iter().map(|selector| selector.specificity).max().unwrap_or_default();
                    Some((Simple::Logical { negated: name == "not", list }, if name == "where" { (0, 0, 0) } else { most }))
                }
                "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
                    let (a, b) = nth(&argument)?;
                    Some((Simple::Nth(a, b, name.contains("last"), name.ends_with("of-type")), one))
                }
                _ => Some((Simple::Never, one)),
            };
        }
        let simple = match name {
            "first-child" => Simple::Nth(0, 1, false, false),
            "last-child" => Simple::Nth(0, 1, true, false),
            "first-of-type" => Simple::Nth(0, 1, false, true),
            "last-of-type" => Simple::Nth(0, 1, true, true),
            "only-child" => {
                return Some((
                    Simple::Logical {
                        negated: false,
                        list: vec![Selector {
                            compounds: vec![(
                                Combinator::Descendant,
                                vec![Simple::Nth(0, 1, false, false), Simple::Nth(0, 1, true, false)],
                            )],
                            pseudo: None,
                            specificity: one,
                        }],
                    },
                    one,
                ));
            }
            "only-of-type" => {
                return Some((
                    Simple::Logical {
                        negated: false,
                        list: vec![Selector {
                            compounds: vec![(Combinator::Descendant, vec![Simple::Nth(0, 1, false, true), Simple::Nth(0, 1, true, true)])],
                            pseudo: None,
                            specificity: one,
                        }],
                    },
                    one,
                ));
            }
            "empty" => Simple::Empty,
            _ => Simple::Never,
        };
        Some((simple, one))
    }
}

/// An `an+b` argument.
fn nth(text: &str) -> Option<(i64, i64)> {
    let text: String = text.split_whitespace().collect::<Vec<_>>().join("").to_ascii_lowercase();
    match text.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    let Some(n) = text.find('n') else { return text.parse().ok().map(|b| (0, b)) };
    let a = match &text[..n] {
        "" | "+" => 1,
        "-" => -1,
        number => number.parse().ok()?,
    };
    let b = if n + 1 == text.len() { 0 } else { text[n + 1..].parse().ok()? };
    Some((a, b))
}

/// A complex selector, or `None` where it is outside what this matcher reads.
fn parse_complex(text: &str) -> Option<Selector> {
    let mut parser = Parser { text: text.trim(), at: 0 };
    let mut compounds: Vec<(Combinator, Vec<Simple>)> = Vec::new();
    let mut combinator = Combinator::Descendant;
    let mut specificity = (0, 0, 0);
    let mut pseudo = None;
    loop {
        let mut compound = Vec::new();
        loop {
            match parser.peek() {
                Some('*') => {
                    parser.bump();
                }
                Some('#') => {
                    parser.bump();
                    compound.push(Simple::Id(parser.identifier()?));
                    specificity.0 += 1;
                }
                Some('.') => {
                    parser.bump();
                    compound.push(Simple::Class(parser.identifier()?));
                    specificity.1 += 1;
                }
                Some('[') => {
                    parser.bump();
                    compound.push(parser.attribute()?);
                    specificity.1 += 1;
                }
                Some(':') => {
                    parser.bump();
                    let element = parser.peek() == Some(':');
                    if element {
                        parser.bump();
                    }
                    let name = parser.identifier()?.to_ascii_lowercase();
                    if element || ["before", "after", "first-line", "first-letter"].contains(&name.as_str()) {
                        if parser.peek() == Some('(') {
                            parser.bump();
                            parser.argument()?;
                        }
                        pseudo = Some(name);
                        specificity.2 += 1;
                    } else {
                        let (simple, weight) = parser.pseudo_class(&name)?;
                        compound.push(simple);
                        specificity = (specificity.0 + weight.0, specificity.1 + weight.1, specificity.2 + weight.2);
                    }
                }
                Some(c) if is_name(c) || c == '\\' => {
                    if !compound.is_empty() || pseudo.is_some() {
                        return None;
                    }
                    compound.push(Simple::Type(parser.identifier()?.to_ascii_lowercase()));
                    specificity.2 += 1;
                }
                _ => break,
            }
        }
        compounds.push((combinator, compound));
        let spaced = parser.spaces();
        let explicit = match parser.peek() {
            None => break,
            Some('>') => Some(Combinator::Child),
            Some('+') => Some(Combinator::Next),
            Some('~') => Some(Combinator::Subsequent),
            Some(_) if spaced => None,
            Some(_) => return None,
        };
        // A pseudo-element ends the selector.
        if pseudo.is_some() {
            return None;
        }
        combinator = match explicit {
            Some(explicit) => {
                parser.bump();
                parser.spaces();
                explicit
            }
            None => Combinator::Descendant,
        };
    }
    Some(Selector { compounds, pseudo, specificity })
}

/// A selector list, or `None` when any member is outside what this matcher reads.
pub(crate) fn parse_list(text: &str) -> Option<Vec<Selector>> {
    split_selector_list(text).iter().map(|member| parse_complex(member)).collect()
}

/// Where an element is: its ancestors, outermost first, and its own place.
#[derive(Debug, Clone, Copy)]
struct At<'a, 'b> {
    ancestors: &'b [&'a Element],
    element: &'a Element,
}

impl<'a> At<'a, '_> {
    fn parent(&self) -> Option<&'a Element> {
        self.ancestors.last().copied()
    }

    fn siblings(&self) -> Vec<&'a Element> {
        self.parent().map(|parent| parent.children.iter().filter_map(Node::as_element).collect()).unwrap_or_default()
    }

    fn position(&self) -> Option<usize> {
        self.siblings().iter().position(|sibling| std::ptr::eq(*sibling, self.element))
    }
}

fn attribute_matches(element: &Element, name: &str, operator: Option<char>, value: &str, insensitive: bool) -> bool {
    let Some(actual) = element.attributes.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str()) else {
        return false;
    };
    let (actual, value) =
        if insensitive { (actual.to_ascii_lowercase(), value.to_ascii_lowercase()) } else { (actual.to_owned(), value.to_owned()) };
    match operator {
        None => true,
        Some('=') => actual == value,
        Some('~') => !value.is_empty() && actual.split_ascii_whitespace().any(|word| word == value),
        Some('|') => actual == value || actual.starts_with(&format!("{value}-")),
        Some('^') => !value.is_empty() && actual.starts_with(&value),
        Some('$') => !value.is_empty() && actual.ends_with(&value),
        Some('*') => !value.is_empty() && actual.contains(&value),
        Some(_) => false,
    }
}

fn simple_matches(simple: &Simple, at: At<'_, '_>) -> bool {
    let element = at.element;
    match simple {
        Simple::Type(name) => element.name == *name,
        Simple::Id(id) => element.attribute("id") == Some(id.as_str()),
        Simple::Class(class) => element.classes().contains(&class.as_str()),
        Simple::Attribute { name, operator, value, insensitive } => attribute_matches(element, name, *operator, value, *insensitive),
        Simple::Logical { negated, list } => list.iter().any(|selector| matches_at(selector, at)) != *negated,
        Simple::Nth(a, b, last, of_type) => {
            let Some(position) = at.position() else { return false };
            let siblings = at.siblings();
            let same = |sibling: &&&Element| !*of_type || sibling.name == element.name;
            let index =
                if *last { siblings[position + 1..].iter().filter(same).count() } else { siblings[..position].iter().filter(same).count() };
            let index = i64::try_from(index).unwrap_or(i64::MAX) + 1;
            if *a == 0 { index == *b } else { (index - b) % a == 0 && (index - b) / a >= 0 }
        }
        Simple::Empty => element.children.iter().all(|child| matches!(child, Node::Text(text) if text.is_empty())),
        Simple::Never => false,
    }
}

fn compound_matches(compound: &[Simple], at: At<'_, '_>) -> bool {
    compound.iter().all(|simple| simple_matches(simple, at))
}

/// Whether compounds `..=last` of `selector` match with compound `last` at `at`.
fn matches_from(selector: &Selector, last: usize, at: At<'_, '_>) -> bool {
    let (combinator, compound) = &selector.compounds[last];
    if !compound_matches(compound, at) {
        return false;
    }
    if last == 0 {
        return true;
    }
    match combinator {
        Combinator::Child => {
            let Some((parent, rest)) = at.ancestors.split_last() else { return false };
            matches_from(selector, last - 1, At { ancestors: rest, element: parent })
        }
        Combinator::Descendant => (0..at.ancestors.len())
            .rev()
            .any(|depth| matches_from(selector, last - 1, At { ancestors: &at.ancestors[..depth], element: at.ancestors[depth] })),
        Combinator::Next | Combinator::Subsequent => {
            let Some(position) = at.position() else { return false };
            let siblings = at.siblings();
            let before = &siblings[..position];
            if *combinator == Combinator::Next {
                before.last().is_some_and(|sibling| matches_from(selector, last - 1, At { ancestors: at.ancestors, element: sibling }))
            } else {
                before.iter().any(|sibling| matches_from(selector, last - 1, At { ancestors: at.ancestors, element: sibling }))
            }
        }
    }
}

fn matches_at(selector: &Selector, at: At<'_, '_>) -> bool {
    !selector.compounds.is_empty() && matches_from(selector, selector.compounds.len() - 1, at)
}

/// Whether `selector` matches `element`, whose ancestors are `ancestors` (outermost first).
pub(crate) fn matches(selector: &Selector, ancestors: &[&Element], element: &Element) -> bool {
    matches_at(selector, At { ancestors, element })
}

// --- The cascade Smaller Anki cards reads ---

/// The computed styles Smaller Anki cards asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Computed {
    pub(crate) display: String,
    pub(crate) visibility: String,
    pub(crate) list_style_type: String,
    pub(crate) margin_left: String,
    pub(crate) margin_right: String,
    pub(crate) vertical_align: String,
    pub(crate) content: String,
    /// Custom properties, which inherit.
    variables: Vec<(String, String)>,
}

impl Default for Computed {
    fn default() -> Self {
        Self {
            display: "inline".to_owned(),
            visibility: "visible".to_owned(),
            list_style_type: "disc".to_owned(),
            margin_left: "0px".to_owned(),
            margin_right: "0px".to_owned(),
            vertical_align: "baseline".to_owned(),
            content: "normal".to_owned(),
            variables: Vec::new(),
        }
    }
}

/// HTML's user-agent display of an element.
fn default_display(element: &Element) -> &'static str {
    if element.has_attribute("hidden") {
        return "none";
    }
    match element.name.as_str() {
        "address" | "blockquote" | "center" | "div" | "figure" | "figcaption" | "footer" | "form" | "header" | "hr" | "legend"
        | "listing" | "main" | "p" | "plaintext" | "pre" | "search" | "xmp" | "details" | "dialog" | "ul" | "ol" | "menu" | "dir"
        | "dl" | "dd" | "dt" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "article" | "aside" | "nav" | "section" | "hgroup"
        | "fieldset" | "body" | "html" => "block",
        "li" | "summary" => "list-item",
        "table" => "table",
        "caption" => "table-caption",
        "colgroup" => "table-column-group",
        "col" => "table-column",
        "thead" => "table-header-group",
        "tbody" => "table-row-group",
        "tfoot" => "table-footer-group",
        "tr" => "table-row",
        "td" | "th" => "table-cell",
        "ruby" => "ruby",
        "rt" => "ruby-text",
        "rp" | "style" | "script" | "head" | "template" | "title" | "meta" | "link" | "base" | "datalist" | "noscript" | "param" => "none",
        _ => "inline",
    }
}

/// Where a declaration stands in the cascade: importance, specificity, then order.
type Precedence = (bool, (u32, u32, u32), usize);

/// The rules of a field's `<style>` elements, with each style rule's selectors.
pub(crate) struct Sheet {
    rules: Vec<(Vec<Selector>, Vec<Declaration>, usize)>,
    /// The selectors of each pseudo-element (`None` for the element itself),
    /// filed by what their last compound requires. Only the selectors an element
    /// could match are tried, as a browser's rule hash does: a field's thousands
    /// of elements, each three times with `::before` and `::after`, against a
    /// dictionary's hundreds of rules would otherwise be millions of matches.
    indexes: Vec<(Option<String>, Index)>,
}

/// A [`Sheet`]'s selectors, as (rule, selector) indices, filed under one thing
/// every element they match has: an id, else a class, else an attribute (its
/// value too when they ask for it exactly), else a tag name.
#[derive(Default)]
struct Index {
    by_id: HashMap<String, Vec<(usize, usize)>>,
    by_class: HashMap<String, Vec<(usize, usize)>>,
    /// By lowercased attribute name, then value.
    by_attribute_value: HashMap<String, HashMap<String, Vec<(usize, usize)>>>,
    /// By lowercased attribute name.
    by_attribute: HashMap<String, Vec<(usize, usize)>>,
    by_type: HashMap<String, Vec<(usize, usize)>>,
    /// The selectors whose last compound requires none of those.
    universal: Vec<(usize, usize)>,
}

impl Index {
    fn file(&mut self, selector: &Selector, at: (usize, usize)) {
        let last = selector.compounds.last().map(|(_, compound)| compound.as_slice()).unwrap_or_default();
        let id = last.iter().find_map(|simple| if let Simple::Id(id) = simple { Some(id) } else { None });
        let class = last.iter().find_map(|simple| if let Simple::Class(class) = simple { Some(class) } else { None });
        let exact = last.iter().find_map(|simple| match simple {
            Simple::Attribute { name, operator: Some('='), value, insensitive: false } => Some((name, value)),
            _ => None,
        });
        let attribute = last.iter().find_map(|simple| if let Simple::Attribute { name, .. } = simple { Some(name) } else { None });
        let name = last.iter().find_map(|simple| if let Simple::Type(name) = simple { Some(name) } else { None });
        let slot = if let Some(id) = id {
            self.by_id.entry(id.clone()).or_default()
        } else if let Some(class) = class {
            self.by_class.entry(class.clone()).or_default()
        } else if let Some((name, value)) = exact {
            self.by_attribute_value.entry(name.to_ascii_lowercase()).or_default().entry(value.clone()).or_default()
        } else if let Some(attribute) = attribute {
            self.by_attribute.entry(attribute.to_ascii_lowercase()).or_default()
        } else if let Some(name) = name {
            self.by_type.entry(name.clone()).or_default()
        } else {
            &mut self.universal
        };
        slot.push(at);
    }

    /// The selectors `element` could match, in sheet order.
    fn candidates(&self, element: &Element) -> Vec<(usize, usize)> {
        let mut found = self.universal.clone();
        if let Some(list) = element.attribute("id").and_then(|id| self.by_id.get(id)) {
            found.extend_from_slice(list);
        }
        for class in element.classes() {
            if let Some(list) = self.by_class.get(class) {
                found.extend_from_slice(list);
            }
        }
        if !self.by_attribute_value.is_empty() || !self.by_attribute.is_empty() {
            for (name, value) in &element.attributes {
                let name = name.to_ascii_lowercase();
                if let Some(list) = self.by_attribute_value.get(&name).and_then(|values| values.get(value)) {
                    found.extend_from_slice(list);
                }
                if let Some(list) = self.by_attribute.get(&name) {
                    found.extend_from_slice(list);
                }
            }
        }
        if let Some(list) = self.by_type.get(&element.name) {
            found.extend_from_slice(list);
        }
        // Attributes whose names differ only in case would list a selector twice.
        found.sort_unstable();
        found.dedup();
        found
    }
}

impl Sheet {
    /// The style rules that apply: those of `@supports`, not of `@media` or `@container`
    /// (which depend on a viewport the relay does not have).
    pub(crate) fn new(text: &str) -> Self {
        fn collect(rules: &[Rule], parent: Option<&str>, out: &mut Vec<(Vec<Selector>, Vec<Declaration>, usize)>) {
            for rule in rules {
                match rule {
                    Rule::Style { selector, declarations, rules } => {
                        let selector = match parent {
                            None => selector.clone(),
                            Some(parent) => nest(parent, selector),
                        };
                        if let Some(list) = parse_list(&selector) {
                            let order = out.len();
                            out.push((list, declarations.clone(), order));
                        }
                        collect(rules, Some(&selector), out);
                    }
                    Rule::Group { name, rules, .. } if name == "supports" => collect(rules, parent, out),
                    Rule::Group { .. } | Rule::Other => {}
                }
            }
        }
        let mut rules = Vec::new();
        collect(&parse_sheet(text), None, &mut rules);
        let mut indexes: Vec<(Option<String>, Index)> = Vec::new();
        for (rule, (list, _, _)) in rules.iter().enumerate() {
            for (index, selector) in list.iter().enumerate() {
                let position = indexes.iter().position(|(pseudo, _)| *pseudo == selector.pseudo).unwrap_or_else(|| {
                    indexes.push((selector.pseudo.clone(), Index::default()));
                    indexes.len() - 1
                });
                indexes[position].1.file(selector, (rule, index));
            }
        }
        Self { rules, indexes }
    }

    /// The selectors of `pseudo` that `element` could match, in sheet order.
    fn candidates(&self, element: &Element, pseudo: Option<&str>) -> Vec<(usize, usize)> {
        self.indexes.iter().find(|(own, _)| own.as_deref() == pseudo).map(|(_, index)| index.candidates(element)).unwrap_or_default()
    }

    /// Rule `best.0`'s declarations, at specificity `best.1`.
    fn apply<'s>(&'s self, best: Option<(usize, (u32, u32, u32))>, matched: &mut Vec<(Precedence, &'s Declaration)>) {
        if let Some((rule, specificity)) = best {
            let (_, declarations, order) = &self.rules[rule];
            for declaration in declarations {
                matched.push(((declaration.important, specificity, *order), declaration));
            }
        }
    }

    /// The rules' declarations that apply to `element` (or its `pseudo`-element), in sheet order:
    /// each rule's once, at the highest specificity of its selectors that match.
    fn matched(&self, ancestors: &[&Element], element: &Element, pseudo: Option<&str>) -> Vec<(Precedence, &Declaration)> {
        let mut matched = Vec::new();
        let mut best: Option<(usize, (u32, u32, u32))> = None;
        for (rule, index) in self.candidates(element, pseudo) {
            let selector = &self.rules[rule].0[index];
            if !matches(selector, ancestors, element) {
                continue;
            }
            match &mut best {
                Some((current, specificity)) if *current == rule => *specificity = (*specificity).max(selector.specificity),
                _ => {
                    self.apply(best, &mut matched);
                    best = Some((rule, selector.specificity));
                }
            }
        }
        self.apply(best, &mut matched);
        matched
    }

    /// The computed style of `element` (or of its `pseudo`-element), given its
    /// parent's.
    pub(crate) fn computed(&self, ancestors: &[&Element], element: &Element, parent: &Computed, pseudo: Option<&str>) -> Computed {
        let mut matched = self.matched(ancestors, element, pseudo);
        let inline: Vec<Declaration> = if pseudo.is_none() {
            element.style().into_iter().map(|(name, value)| Declaration { name, value, important: false }).collect()
        } else {
            Vec::new()
        };
        for declaration in &inline {
            matched.push(((false, (u32::MAX, 0, 0), usize::MAX), declaration));
        }
        matched.sort_by_key(|(key, _)| *key);
        let mut style = Computed {
            visibility: parent.visibility.clone(),
            list_style_type: parent.list_style_type.clone(),
            variables: parent.variables.clone(),
            ..Computed::default()
        };
        if pseudo.is_none() {
            default_display(element).clone_into(&mut style.display);
            if ["ul", "ol", "menu", "dir"].contains(&element.name.as_str()) {
                ua_marker(ancestors, element).clone_into(&mut style.list_style_type);
            }
            if element.name == "summary" {
                "disclosure-closed".clone_into(&mut style.list_style_type);
            }
        }
        // Custom properties first, so declarations can read them.
        for (_, declaration) in &matched {
            if declaration.name.starts_with("--") {
                let value = substitute(&declaration.value, &style.variables);
                style.variables.retain(|(name, _)| *name != declaration.name);
                style.variables.push((declaration.name.clone(), value));
            }
        }
        for (_, declaration) in matched {
            let value = substitute(&declaration.value, &style.variables).to_ascii_lowercase();
            let inherit = |own: &str| if value == "inherit" { Some(own.to_owned()) } else { None };
            match declaration.name.as_str() {
                "display" => style.display = inherit(&parent.display).unwrap_or_else(|| value.clone()),
                "visibility" => style.visibility = inherit(&parent.visibility).unwrap_or_else(|| value.clone()),
                "list-style-type" => style.list_style_type = inherit(&parent.list_style_type).unwrap_or_else(|| value.clone()),
                "list-style" => {
                    if let Some(kind) = value.split_whitespace().find(|word| !is_list_position(word) && !word.contains('(')) {
                        kind.clone_into(&mut style.list_style_type);
                    }
                }
                "margin-left" | "margin-inline-start" => style.margin_left.clone_from(&value),
                "margin-right" | "margin-inline-end" => style.margin_right.clone_from(&value),
                "margin" => {
                    let words: Vec<&str> = value.split_whitespace().collect();
                    let (left, right) = match words.as_slice() {
                        [all] => (*all, *all),
                        [_, sides] | [_, sides, _] => (*sides, *sides),
                        [_, right, _, left, ..] => (*left, *right),
                        [] => ("0px", "0px"),
                    };
                    (style.margin_left, style.margin_right) = (left.to_owned(), right.to_owned());
                }
                "margin-inline" => {
                    let words: Vec<&str> = value.split_whitespace().collect();
                    if let Some(first) = words.first() {
                        (*first).clone_into(&mut style.margin_left);
                        (*words.get(1).unwrap_or(first)).clone_into(&mut style.margin_right);
                    }
                }
                "vertical-align" => style.vertical_align.clone_from(&value),
                // `content` keeps its case: its strings are the text.
                "content" => style.content = substitute(&declaration.value, &style.variables),
                _ => {}
            }
        }
        style
    }
}

fn is_list_position(word: &str) -> bool {
    matches!(word, "inside" | "outside")
}

/// A nested rule's selector under its parent's.
fn nest(parent: &str, selector: &str) -> String {
    split_selector_list(selector)
        .iter()
        .flat_map(|member| {
            split_selector_list(parent).into_iter().map(move |outer| {
                if member.contains('&') { member.replace('&', &format!(":is({outer})")) } else { format!(":is({outer}) {member}") }
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// HTML's list marker for a list, by how deeply it is nested in other lists.
fn ua_marker(ancestors: &[&Element], element: &Element) -> &'static str {
    if element.name == "ol" {
        return "decimal";
    }
    let depth = ancestors.iter().filter(|ancestor| ["ul", "ol", "menu", "dir"].contains(&ancestor.name.as_str())).count();
    match depth {
        0 => "disc",
        1 => "circle",
        _ => "square",
    }
}

/// `var()` references replaced by their values (or fallbacks).
fn substitute(value: &str, variables: &[(String, String)]) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.to_ascii_lowercase().find("var(") {
        out.push_str(&rest[..start]);
        let inner_start = start + 4;
        let mut parser = Parser { text: rest, at: inner_start };
        let Some(argument) = parser.argument() else { break };
        let parts = dom::split_top_level(argument, ',');
        let name = parts[0].trim();
        let fallback = (parts.len() > 1).then(|| argument[parts[0].len() + 1..].trim());
        let resolved = variables.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        out.push_str(&resolved.or_else(|| fallback.map(|fallback| substitute(fallback, variables))).unwrap_or_default());
        rest = &rest[parser.at..];
    }
    out.push_str(rest);
    out
}

/// The text of a computed `content` value's strings, up to an alternative text.
pub(crate) fn generated_text(content: &str) -> String {
    let mut text = String::new();
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' => break,
            '"' | '\'' => {
                while let Some(next) = chars.next() {
                    if next == c {
                        break;
                    }
                    if next != '\\' {
                        text.push(next);
                        continue;
                    }
                    let mut hex = String::new();
                    while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
                        hex.extend(chars.next());
                    }
                    if hex.is_empty() {
                        match chars.next() {
                            Some('\n') | None => {}
                            Some(escaped) => text.push(escaped),
                        }
                    } else {
                        if chars.peek() == Some(&' ') {
                            chars.next();
                        }
                        text.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).filter(|c| *c != '\0').unwrap_or('\u{fffd}'));
                    }
                }
            }
            _ => {}
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_filters_and_renames_dictionary_styles() {
        let styles = "/* c */ .a, .b > i { color: red; --x: 1px; margin: var(--x) } \
            @font-face { font-family: X; src: url(x) } @import 'x'; \
            .bad { background: url(x.png) } .font { font-family: \"Noto\", serif } .ok { font: 12px serif; color: var(--fg) } \
            @media (max-width: 1px) { .m { display: none } } .n { color: blue; .c { color: green } }";
        assert_eq!(
            scoped_sheet(styles, "--hdP-", ".yomitan-glossary [data-dictionary=\"D\"]"),
            ".yomitan-glossary [data-dictionary=\"D\"] .a, .yomitan-glossary [data-dictionary=\"D\"] .b > i { color: red; --hdP-x: 1px; margin: var(--hdP-x); }\n\
             .yomitan-glossary [data-dictionary=\"D\"] .ok { font: 12px serif; color: color-mix(in srgb, var(--fg) 100%, transparent); }\n\
             @media (max-width: 1px) {\n  .yomitan-glossary [data-dictionary=\"D\"] .m { display: none; }\n}\n\
             .yomitan-glossary [data-dictionary=\"D\"] .n {\n  color: blue;\n  .c { color: green; }\n}"
        );
    }

    #[test]
    fn finds_unsafe_functions() {
        assert!(unsafe_function("background: URL (x)"));
        assert!(unsafe_function("x: --custom(1)"));
        assert!(!unsafe_function("x: a--custom(1)"));
        assert!(!unsafe_function("x: myurl(1)"));
        assert!(has_var("width: var (--x)"));
        assert!(!has_var("width: avar(--x)"));
    }

    fn tree() -> Element {
        let mut root = Element::with_class("div", "yomitan-glossary");
        let mut list = Element::new("ol");
        for index in 0..3 {
            let mut item = Element::with_class("li", "entry");
            item.set_attribute("data-dictionary", if index == 1 { "B" } else { "A" });
            item.append(Element::with_class("span", "gloss-image-link"));
            list.append(item);
        }
        root.append(list);
        root
    }

    #[test]
    fn matches_selectors() {
        let root = tree();
        let list = root.children[0].as_element().unwrap();
        let items: Vec<&Element> = list.children.iter().filter_map(Node::as_element).collect();
        let span = items[2].children[0].as_element().unwrap();
        let ancestors = [&root, list, items[2]];
        let test = |text: &str| parse_list(text).unwrap().iter().any(|selector| matches(selector, &ancestors, span));
        assert!(test(".yomitan-glossary [data-dictionary=A] .gloss-image-link"));
        assert!(test(".entry:nth-last-of-type(1):not(:nth-of-type(1)) .gloss-image-link"));
        assert!(test("li[data-dictionary=\"B\"] ~ li > span"));
        assert!(!test("li[data-dictionary=B] + li + li + li span"));
        assert!(test("li:nth-child(2n+1) span:only-child"));
        assert!(!test(":root .gloss-image-link"));
        assert!(!test(".gloss-image-link:hover"));
        assert!(test("ol > :is(.x, .entry):last-child span"));
    }

    #[test]
    fn cascades_what_compact_reads() {
        let sheet = Sheet::new(
            ".x { display: block; --m: 2px } .x span { margin: 0 var(--m) } span::before { content: \"\\2022 a\" / \"alt\" } \
             @media print { .x { display: none } } li { list-style: square inside }",
        );
        let mut x = Element::with_class("div", "x");
        x.append(Element::new("span"));
        let span = x.children[0].as_element().unwrap();
        let top = sheet.computed(&[], &x, &Computed::default(), None);
        assert_eq!(top.display, "block");
        let inner = sheet.computed(&[&x], span, &top, None);
        assert_eq!((inner.display.as_str(), inner.margin_left.as_str(), inner.margin_right.as_str()), ("inline", "2px", "2px"));
        let before = sheet.computed(&[&x], span, &inner, Some("before"));
        assert_eq!(generated_text(&before.content), "\u{2022}a");
        let li = Element::new("li");
        assert_eq!(sheet.computed(&[], &li, &Computed::default(), None).list_style_type, "square");
    }

    /// What `computed` matched before the index: every rule, in sheet order.
    fn scanned<'s>(
        sheet: &'s Sheet,
        ancestors: &[&Element],
        element: &Element,
        pseudo: Option<&str>,
    ) -> Vec<(Precedence, &'s Declaration)> {
        let mut matched = Vec::new();
        for (list, declarations, order) in &sheet.rules {
            let specificity = list
                .iter()
                .filter(|selector| selector.pseudo.as_deref() == pseudo && matches(selector, ancestors, element))
                .map(|selector| selector.specificity)
                .max();
            if let Some(specificity) = specificity {
                matched.extend(declarations.iter().map(|declaration| ((declaration.important, specificity, *order), declaration)));
            }
        }
        matched
    }

    #[test]
    fn the_index_tries_every_selector_that_could_match() {
        let sheet = Sheet::new(
            "[data-sc-content=\"gloss\"] { display: block } [data-x] { margin-left: 3px } [data-v=\"A\" i] { margin-right: 4px } \
             span#n, .c { vertical-align: top } span.c { vertical-align: sub } * { visibility: visible } \
             .c::after { content: \"z\" } div > :is(.c, [data-x]) { margin: 1px } span:not(.c) { display: inline-block } \
             ol li span[data-sc-content] { margin-left: 5px }",
        );
        let mut root = Element::new("ol");
        let mut item = Element::new("li");
        let attributed = |pairs: &[(&str, &str)]| {
            let mut span = Element::new("span");
            span.attributes = pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
            span
        };
        for span in [
            attributed(&[("data-sc-content", "gloss")]),
            attributed(&[("DATA-SC-CONTENT", "gloss")]),
            attributed(&[("data-sc-content", "other"), ("data-x", "")]),
            attributed(&[("data-v", "a"), ("class", "c c")]),
            attributed(&[("id", "n"), ("class", "c")]),
            attributed(&[]),
        ] {
            item.append(span);
        }
        root.append(item);
        let mut wrapper = Element::new("div");
        wrapper.append(attributed(&[("data-x", "1"), ("class", "c")]));
        let mut all: Vec<(Vec<&Element>, &Element)> = vec![(vec![], &root), (vec![&root], root.children[0].as_element().unwrap())];
        let item = root.children[0].as_element().unwrap();
        all.extend(item.children.iter().filter_map(Node::as_element).map(|span| (vec![&root, item], span)));
        all.extend(wrapper.children.iter().filter_map(Node::as_element).map(|span| (vec![&wrapper], span)));
        for (ancestors, element) in &all {
            for pseudo in [None, Some("before"), Some("after")] {
                let indexed = sheet.matched(ancestors, element, pseudo);
                assert_eq!(indexed, scanned(&sheet, ancestors, element, pseudo), "{} {:?} {pseudo:?}", element.name, element.attributes);
            }
        }
        // A rule applies at its best matching selector: span#n beats the later span.c, .c alone does not.
        let spans = &item.children;
        let style = |index: usize| sheet.computed(&[&root, item], spans[index].as_element().unwrap(), &Computed::default(), None);
        assert_eq!(style(4).vertical_align, "top");
        assert_eq!(style(3).vertical_align, "sub");
        // An attribute name matches in any case; span:not(.c) outweighs the attribute alone.
        assert_eq!((style(1).display.as_str(), style(1).margin_left.as_str()), ("inline-block", "5px"));
        assert_eq!(style(3).margin_right, "4px");
    }

    #[test]
    fn serializes_strings() {
        assert_eq!(serialize_string("a\"b\\"), "\"a\\\"b\\\\\"");
    }
}

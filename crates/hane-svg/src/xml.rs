//! A namespace-aware XML parser, covering the subset SVG documents use.
//!
//! [`parse`] returns the root [`Element`] of an owned tree. A tree rather than
//! an event stream because SVG import resolves inherited presentation
//! attributes: a walk needs its parent chain in hand, which a caller of a pull
//! parser has to rebuild by keeping a stack of its own. Owned `String`s rather
//! than borrows of the input because entity expansion, attribute-value
//! normalisation and line-end normalisation all produce text that is not a
//! substring of the source anyway.
//!
//! # What this deliberately does not do
//!
//! - **No DTD processing.** `<!DOCTYPE ...>` is skipped whole, internal subset
//!   included, so entity *declarations* never take effect and only the five
//!   predefined entities plus character references resolve. That is what makes
//!   the billion-laughs expansion attack impossible here rather than merely
//!   bounded: every reference expands to at most four bytes, so the output is
//!   strictly smaller than the input.
//! - **No external entities, and no I/O of any kind.** A design tool opening an
//!   untrusted file must not fetch anything.
//! - **No validation, no XSLT, no XInclude, no `xml:base`.**
//! - Comments and processing instructions are skipped rather than kept; nothing
//!   downstream renders them and SVG export writes its own.

use core::fmt;

/// The namespace the `xml` prefix is bound to without being declared.
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Nesting deeper than this is rejected instead of parsed.
///
/// Both the parser and the `Vec<Node>` destructor recurse, so an adversarial
/// `<a><a><a>...` would overflow the stack -- and a stack overflow aborts,
/// which is exactly the "no panic on malformed input" property this crate
/// claims. Real SVG nests groups a handful deep.
const MAX_DEPTH: u32 = 256;

/// An expanded name: the namespace it resolved to, plus the local part.
///
/// `namespace` is `None` for a name in no namespace, which is the normal case
/// for attributes -- an unprefixed attribute does *not* take the default
/// namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    /// The namespace URI, or `None` when the name is in no namespace.
    pub namespace: Option<String>,
    /// The part after the prefix.
    pub local: String,
}

/// One attribute of an element, with its value already normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    /// The resolved attribute name.
    pub name: Name,
    /// The value, after entity expansion and whitespace normalisation.
    pub value: String,
}

/// A child of an element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A nested element.
    Element(Element),
    /// Character data. Adjacent text, CDATA and entity runs are merged into one
    /// node, so a caller never has to stitch fragments back together.
    Text(String),
}

/// An element and everything under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    /// The resolved element name.
    pub name: Name,
    /// Attributes in document order, minus the `xmlns` declarations, which are
    /// consumed into the resolved names.
    pub attributes: Vec<Attribute>,
    /// Children in document order.
    pub children: Vec<Node>,
    /// 1-based line of the `<` that opened this element.
    pub line: usize,
    /// 1-based column of the `<` that opened this element.
    pub column: usize,
}

impl Element {
    /// The value of the unprefixed attribute `local`, if present.
    ///
    /// Presentation attributes -- `fill`, `transform`, `d` -- are always
    /// unprefixed, so this is the lookup import actually makes. Anything
    /// namespaced (`xlink:href`) is found by scanning [`attributes`](Self::attributes).
    pub fn attribute(&self, local: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|a| a.name.namespace.is_none() && a.name.local == local)
            .map(|a| a.value.as_str())
    }
}

/// What went wrong, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// A description of the failure, without position.
    pub message: String,
    /// 1-based line where the parser stopped.
    pub line: usize,
    /// 1-based column, counted in characters, where the parser stopped.
    pub column: usize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for Error {}

/// Parse a whole document and return its root element.
///
/// # Errors
///
/// Returns the first malformedness found, with the line and column at which the
/// parser noticed it. Malformed input never panics.
pub fn parse(input: &str) -> Result<Element, Error> {
    // Line-end normalisation (spec 2.11) is defined over the whole entity, so
    // it happens once here rather than at every point text is produced. The
    // copy is skipped entirely for the usual case of no carriage returns.
    let owned;
    let mut text = input.strip_prefix('\u{feff}').unwrap_or(input);
    if text.contains('\r') {
        owned = text.replace("\r\n", "\n").replace('\r', "\n");
        text = &owned;
    }

    let mut parser = Parser {
        input: text,
        pos: 0,
        line: 1,
        column: 1,
        scopes: Vec::new(),
    };
    parser.skip_misc()?;
    let root = parser.parse_element(0)?;
    parser.skip_misc()?;
    if parser.pos < parser.input.len() {
        return parser.err("trailing content after the root element");
    }
    Ok(root)
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    line: usize,
    column: usize,
    /// In-scope namespace declarations as `(prefix, uri)`, innermost last. The
    /// empty prefix is the default namespace, and an empty uri is `xmlns=""`,
    /// which undeclares it.
    scopes: Vec<(String, String)>,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        self.err_at(self.line, self.column, message)
    }

    fn err_at<T>(
        &self,
        line: usize,
        column: usize,
        message: impl Into<String>,
    ) -> Result<T, Error> {
        Err(Error {
            message: message.into(),
            line,
            column,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.input[self.pos..].starts_with(s)
    }

    /// Consume `n` bytes, keeping line and column current.
    ///
    /// Tracking the position incrementally rather than recomputing it from the
    /// byte offset keeps per-element positions from costing O(n) each.
    fn advance(&mut self, n: usize) {
        for &b in &self.input.as_bytes()[self.pos..self.pos + n] {
            if b == b'\n' {
                self.line += 1;
                self.column = 1;
            } else if b & 0xc0 != 0x80 {
                // UTF-8 continuation bytes are not separate characters.
                self.column += 1;
            }
        }
        self.pos += n;
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.advance(1);
        }
    }

    /// Consume up to and including `end`, returning what came before it.
    fn take_until(&mut self, end: &str, what: &str) -> Result<&'a str, Error> {
        match self.input[self.pos..].find(end) {
            Some(i) => {
                let inner = &self.input[self.pos..self.pos + i];
                self.advance(i + end.len());
                Ok(inner)
            }
            None => self.err(format!("unterminated {what}")),
        }
    }

    /// Whitespace, comments, processing instructions and the doctype, in any
    /// order -- everything that may appear outside the root element.
    fn skip_misc(&mut self) -> Result<(), Error> {
        loop {
            self.skip_whitespace();
            if self.starts_with("<!--") {
                self.advance(4);
                self.take_until("-->", "comment")?;
            } else if self.starts_with("<?") {
                self.advance(2);
                self.take_until("?>", "processing instruction")?;
            } else if self.starts_with("<!DOCTYPE") {
                self.skip_doctype()?;
            } else {
                return Ok(());
            }
        }
    }

    /// Skip `<!DOCTYPE ...>` whole, internal subset included.
    ///
    /// Nothing inside is interpreted: see the module docs on why declared
    /// entities are not honoured.
    fn skip_doctype(&mut self) -> Result<(), Error> {
        self.advance("<!DOCTYPE".len());
        let mut in_subset = false;
        loop {
            match self.peek() {
                None => return self.err("unterminated doctype declaration"),
                Some(q @ (b'"' | b'\'')) => {
                    self.advance(1);
                    // A literal may hold `>` or `[`, so it is skipped as a unit.
                    let end = if q == b'"' { "\"" } else { "'" };
                    self.take_until(end, "literal in the doctype")?;
                }
                Some(b'[') => {
                    in_subset = true;
                    self.advance(1);
                }
                Some(b']') => {
                    in_subset = false;
                    self.advance(1);
                }
                Some(b'>') if !in_subset => {
                    self.advance(1);
                    return Ok(());
                }
                Some(_) => self.advance(1),
            }
        }
    }

    /// Scan a qualified name and return it raw, prefix included.
    fn scan_qname(&mut self) -> Result<&'a str, Error> {
        let start = self.pos;
        match self.peek() {
            Some(b) if is_name_start(b) => self.advance(1),
            _ => return self.err("expected a name"),
        }
        while self.peek().is_some_and(is_name_byte) {
            self.advance(1);
        }
        Ok(&self.input[start..self.pos])
    }

    /// Split a qualified name into `(prefix, local)`; the prefix is empty when
    /// there is none.
    fn split_qname(&self, qname: &'a str) -> Result<(&'a str, &'a str), Error> {
        match qname.split_once(':') {
            None => Ok(("", qname)),
            Some((prefix, local))
                if !prefix.is_empty() && !local.is_empty() && !local.contains(':') =>
            {
                Ok((prefix, local))
            }
            Some(_) => self.err(format!("`{qname}` is not a valid qualified name")),
        }
    }

    fn lookup(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NS);
        }
        self.scopes
            .iter()
            .rev()
            .find(|(p, _)| p == prefix)
            .map(|(_, uri)| uri.as_str())
            .filter(|uri| !uri.is_empty())
    }

    /// Resolve a qualified name against the in-scope declarations.
    ///
    /// `default_ns` is false for attributes: an unprefixed attribute is in no
    /// namespace, never in the default one.
    fn resolve(
        &self,
        qname: &'a str,
        default_ns: bool,
        line: usize,
        column: usize,
    ) -> Result<Name, Error> {
        let (prefix, local) = self.split_qname(qname)?;
        let namespace = if prefix.is_empty() {
            if default_ns {
                self.lookup("").map(str::to_string)
            } else {
                None
            }
        } else {
            match self.lookup(prefix) {
                Some(uri) => Some(uri.to_string()),
                None => {
                    return self.err_at(
                        line,
                        column,
                        format!("unbound namespace prefix `{prefix}`"),
                    );
                }
            }
        };
        Ok(Name {
            namespace,
            local: local.to_string(),
        })
    }

    fn parse_element(&mut self, depth: u32) -> Result<Element, Error> {
        if depth > MAX_DEPTH {
            return self.err(format!("elements nested more than {MAX_DEPTH} deep"));
        }
        let (line, column) = (self.line, self.column);
        if !self.starts_with("<") {
            return self.err("expected an element");
        }
        self.advance(1);
        let qname = self.scan_qname()?;

        // Attributes are collected raw first: a declaration made on this element
        // is in scope for its own attributes, so nothing can be resolved until
        // every `xmlns` on the tag has been seen.
        let scope_base = self.scopes.len();
        let mut raw: Vec<(&str, String, usize, usize)> = Vec::new();
        let empty;
        loop {
            let before = self.pos;
            self.skip_whitespace();
            let had_space = self.pos > before;
            if self.starts_with("/>") {
                self.advance(2);
                empty = true;
                break;
            }
            if self.peek() == Some(b'>') {
                self.advance(1);
                empty = false;
                break;
            }
            if !had_space {
                return self.err(format!(
                    "expected whitespace or `>` in the `{qname}` start tag"
                ));
            }
            let (aline, acolumn) = (self.line, self.column);
            let aname = self.scan_qname()?;
            self.skip_whitespace();
            if self.peek() != Some(b'=') {
                return self.err(format!("expected `=` after attribute `{aname}`"));
            }
            self.advance(1);
            self.skip_whitespace();
            let value = self.scan_attribute_value()?;

            match self.split_qname(aname)? {
                ("", "xmlns") => self.scopes.push((String::new(), value)),
                ("xmlns", prefix) => {
                    // Both reserved prefixes are fixed by the namespaces spec.
                    if prefix == "xmlns" || (prefix == "xml" && value != XML_NS) {
                        return self.err_at(
                            aline,
                            acolumn,
                            format!("the prefix `{prefix}` cannot be bound to `{value}`"),
                        );
                    }
                    if value.is_empty() {
                        return self.err_at(
                            aline,
                            acolumn,
                            format!("cannot undeclare the namespace prefix `{prefix}`"),
                        );
                    }
                    self.scopes.push((prefix.to_string(), value));
                }
                _ => raw.push((aname, value, aline, acolumn)),
            }
        }

        let name = self.resolve(qname, true, line, column)?;
        let mut attributes = Vec::with_capacity(raw.len());
        for (aname, value, aline, acolumn) in raw {
            let name = self.resolve(aname, false, aline, acolumn)?;
            // ponytail: quadratic duplicate check. Elements carry a handful of
            // attributes; a set would cost more than it saves.
            if attributes.iter().any(|a: &Attribute| a.name == name) {
                return self.err_at(aline, acolumn, format!("duplicate attribute `{aname}`"));
            }
            attributes.push(Attribute { name, value });
        }

        let children = if empty {
            Vec::new()
        } else {
            self.parse_children(qname, line, column, depth)?
        };
        self.scopes.truncate(scope_base);
        Ok(Element {
            name,
            attributes,
            children,
            line,
            column,
        })
    }

    /// Content up to and including the end tag matching `qname`.
    fn parse_children(
        &mut self,
        qname: &str,
        line: usize,
        column: usize,
        depth: u32,
    ) -> Result<Vec<Node>, Error> {
        let mut children: Vec<Node> = Vec::new();
        loop {
            if self.starts_with("</") {
                self.advance(2);
                let (eline, ecolumn) = (self.line, self.column);
                let end = self.scan_qname()?;
                if end != qname {
                    return self.err_at(
                        eline,
                        ecolumn,
                        format!(
                            "`</{end}>` closes `<{qname}>` opened at line {line}, column {column}"
                        ),
                    );
                }
                self.skip_whitespace();
                if self.peek() != Some(b'>') {
                    return self.err(format!("expected `>` to close `</{end}`"));
                }
                self.advance(1);
                return Ok(children);
            }
            if self.starts_with("<![CDATA[") {
                self.advance("<![CDATA[".len());
                let text = self.take_until("]]>", "CDATA section")?;
                push_text(&mut children, text);
            } else if self.starts_with("<!--") {
                self.advance(4);
                self.take_until("-->", "comment")?;
            } else if self.starts_with("<?") {
                self.advance(2);
                self.take_until("?>", "processing instruction")?;
            } else if self.starts_with("<!") {
                return self.err("declarations are only allowed before the root element");
            } else if self.peek() == Some(b'<') {
                let child = self.parse_element(depth + 1)?;
                children.push(Node::Element(child));
            } else if self.peek().is_none() {
                return self.err(format!(
                    "unexpected end of input: `<{qname}>` opened at line {line}, column {column} is never closed"
                ));
            } else {
                let mut text = String::new();
                self.scan_chars(&mut text, None)?;
                push_text(&mut children, &text);
            }
        }
    }

    /// A quoted attribute value, expanded and normalised.
    fn scan_attribute_value(&mut self) -> Result<String, Error> {
        let quote = match self.peek() {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return self.err("expected a quoted attribute value"),
        };
        self.advance(1);
        let mut value = String::new();
        self.scan_chars(&mut value, Some(quote))?;
        if self.peek() != Some(quote) {
            return self.err("unterminated attribute value");
        }
        self.advance(1);
        Ok(value)
    }

    /// Character data with references expanded, stopping at `<` or, inside an
    /// attribute value, at the closing `quote`.
    fn scan_chars(&mut self, out: &mut String, quote: Option<u8>) -> Result<(), Error> {
        loop {
            match self.peek() {
                None => return Ok(()),
                Some(b) if Some(b) == quote => return Ok(()),
                Some(b'<') if quote.is_some() => {
                    return self.err("`<` is not allowed in an attribute value");
                }
                Some(b'<') => return Ok(()),
                Some(b'&') => self.expand_reference(out)?,
                // Attribute-value normalisation (spec 3.3.3): a *literal* tab or
                // newline becomes a space, but the same character written as a
                // reference survives -- which is why this is here and not in
                // `expand_reference`. Carriage returns are already newlines.
                Some(b'\t' | b'\n') if quote.is_some() => {
                    out.push(' ');
                    self.advance(1);
                }
                // The `Char` production (spec 2.2) excludes the C0 controls, so
                // a stray NUL is malformedness rather than content.
                Some(b) if b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r') => {
                    return self.err("control character in character data");
                }
                Some(_) => {
                    let start = self.pos;
                    while let Some(b) = self.peek() {
                        if b == b'<' || b == b'&' || Some(b) == quote {
                            break;
                        }
                        if b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r') {
                            break;
                        }
                        if quote.is_some() && (b == b'\t' || b == b'\n') {
                            break;
                        }
                        self.advance(1);
                    }
                    out.push_str(&self.input[start..self.pos]);
                }
            }
        }
    }

    /// Expand one `&...;`.
    ///
    /// Only the five predefined entities and character references resolve, and
    /// each yields at most four bytes, so expansion is bounded by the input
    /// length -- there is no billion-laughs case to guard against.
    fn expand_reference(&mut self, out: &mut String) -> Result<(), Error> {
        let (line, column) = (self.line, self.column);
        self.advance(1);
        if self.peek() == Some(b'#') {
            self.advance(1);
            let hex = self.peek() == Some(b'x');
            if hex {
                self.advance(1);
            }
            let start = self.pos;
            let mut value: u32 = 0;
            while let Some(b) = self.peek() {
                let digit = match b {
                    b'0'..=b'9' => u32::from(b - b'0'),
                    b'a'..=b'f' if hex => u32::from(b - b'a') + 10,
                    b'A'..=b'F' if hex => u32::from(b - b'A') + 10,
                    _ => break,
                };
                // Saturating, so a run of a thousand digits stays a number
                // rather than wrapping into a valid code point.
                value = value
                    .saturating_mul(if hex { 16 } else { 10 })
                    .saturating_add(digit);
                self.advance(1);
            }
            if self.pos == start || self.peek() != Some(b';') {
                return self.err_at(line, column, "malformed character reference");
            }
            self.advance(1);
            match char::from_u32(value).filter(|c| is_xml_char(*c)) {
                Some(c) => out.push(c),
                None => {
                    return self.err_at(
                        line,
                        column,
                        format!("character reference to {value} is not a valid XML character"),
                    );
                }
            }
            return Ok(());
        }
        let name = self.scan_qname()?;
        if self.peek() != Some(b';') {
            return self.err_at(
                line,
                column,
                format!("unterminated entity reference `&{name}`"),
            );
        }
        self.advance(1);
        out.push(match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            // Declared entities are not honoured: see the module docs.
            _ => {
                return self.err_at(line, column, format!("unknown entity `&{name};`"));
            }
        });
        Ok(())
    }
}

/// Append to the trailing text node when there is one, so a run of text, CDATA
/// and entities arrives as a single [`Node::Text`].
fn push_text(children: &mut Vec<Node>, text: &str) {
    if text.is_empty() {
        return;
    }
    match children.last_mut() {
        Some(Node::Text(existing)) => existing.push_str(text),
        _ => children.push(Node::Text(text.to_string())),
    }
}

/// ponytail: the real `NameStartChar` production is a dozen code point ranges.
/// Every byte above ASCII is accepted instead, which admits some names XML
/// would reject and rejects none it allows -- a laxness no SVG file can tell
/// apart. Tighten it if a conformance suite ever cares.
fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b':' || b >= 0x80
}

fn is_name_byte(b: u8) -> bool {
    is_name_start(b) || b.is_ascii_digit() || b == b'-' || b == b'.'
}

/// The `Char` production (spec 2.2): most control characters are not legal XML.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | ' '..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::{Rng, check};

    fn root(input: &str) -> Element {
        parse(input).expect("should parse")
    }

    fn text_of(element: &Element) -> String {
        element
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) => Some(t.as_str()),
                Node::Element(_) => None,
            })
            .collect()
    }

    #[test]
    fn parses_nesting_and_positions() {
        let e = root("<a>\n  <b x='1'/>\n</a>");
        assert_eq!(e.name.local, "a");
        assert_eq!((e.line, e.column), (1, 1));
        let Node::Element(b) = &e.children[1] else {
            panic!("expected an element child, got {:?}", e.children);
        };
        assert_eq!(b.attribute("x"), Some("1"));
        assert_eq!((b.line, b.column), (2, 3));
        assert!(b.children.is_empty());
    }

    #[test]
    fn default_and_prefixed_namespaces_resolve() {
        let e = root(
            "<svg xmlns='http://www.w3.org/2000/svg' \
             xmlns:xlink='http://www.w3.org/1999/xlink'>\
             <use xlink:href='#x' fill='red'/></svg>",
        );
        assert_eq!(
            e.name.namespace.as_deref(),
            Some("http://www.w3.org/2000/svg")
        );
        let Node::Element(u) = &e.children[0] else {
            panic!("expected an element child");
        };
        // The element inherits the default namespace; the unprefixed attribute
        // does not take it, but the prefixed one resolves.
        assert_eq!(
            u.name.namespace.as_deref(),
            Some("http://www.w3.org/2000/svg")
        );
        assert_eq!(
            u.attributes[0].name.namespace.as_deref(),
            Some("http://www.w3.org/1999/xlink")
        );
        assert_eq!(u.attributes[0].name.local, "href");
        assert_eq!(u.attributes[1].name.namespace, None);
    }

    #[test]
    fn namespaces_redefine_on_nested_elements_and_pop() {
        let e = root(
            "<a xmlns='one' xmlns:p='two'>\
             <b xmlns='three' xmlns:p='four'><p:c/></b>\
             <d/><e xmlns=''/></a>",
        );
        let child = |i: usize| match &e.children[i] {
            Node::Element(c) => c.clone(),
            Node::Text(t) => panic!("expected an element, got {t:?}"),
        };
        let b = child(0);
        assert_eq!(b.name.namespace.as_deref(), Some("three"));
        let Node::Element(c) = &b.children[0] else {
            panic!("expected an element child");
        };
        assert_eq!(c.name.namespace.as_deref(), Some("four"));
        // Back outside `b`, the outer declarations are in scope again.
        assert_eq!(child(1).name.namespace.as_deref(), Some("one"));
        // `xmlns=""` undeclares the default namespace.
        assert_eq!(child(2).name.namespace, None);
    }

    #[test]
    fn xml_prefix_is_predeclared() {
        let e = root("<a xml:space='preserve'/>");
        assert_eq!(e.attributes[0].name.namespace.as_deref(), Some(XML_NS));
    }

    #[test]
    fn unbound_prefix_reports_the_element_position() {
        let err = parse("<a>\n  <p:b/>\n</a>").unwrap_err();
        assert_eq!((err.line, err.column), (2, 3));
        assert!(
            err.message.contains("unbound namespace prefix `p`"),
            "{err}"
        );
    }

    #[test]
    fn predefined_and_numeric_entities_expand() {
        let e = root("<a t='&lt;&amp;&gt;&quot;&apos;'>&#65;&#x41;&#x1f600;</a>");
        assert_eq!(e.attribute("t"), Some("<&>\"'"));
        assert_eq!(text_of(&e), "AA\u{1f600}");
    }

    #[test]
    fn undeclared_entities_do_not_expand() {
        // The billion-laughs shape: the declarations are skipped with the
        // doctype, so the reference is simply unknown.
        let doc = "<!DOCTYPE a [<!ENTITY x 'aaaa'><!ENTITY y '&x;&x;&x;'>]><a>&y;</a>";
        let err = parse(doc).unwrap_err();
        assert!(err.message.contains("unknown entity `&y;`"), "{err}");
    }

    #[test]
    fn invalid_character_references_are_rejected() {
        for doc in [
            "<a>&#xD800;</a>",   // a surrogate
            "<a>&#x110000;</a>", // beyond the last code point
            "<a>&#0;</a>",       // NUL is not a valid XML character
            "<a>&#;</a>",
            "<a>&#99999999999999999999;</a>",
            "<a>&amp</a>",
        ] {
            assert!(parse(doc).is_err(), "{doc} should not parse");
        }
    }

    #[test]
    fn cdata_is_literal_and_merges_with_surrounding_text() {
        let e = root("<a>x <![CDATA[ <b> & ]]>&amp; y</a>");
        assert_eq!(e.children.len(), 1);
        assert_eq!(text_of(&e), "x  <b> & & y");
    }

    #[test]
    fn attribute_values_are_normalised() {
        // Literal tab and newline become spaces; the same characters written as
        // references survive, and so does a literal run of spaces.
        let e = root("<a v='p\tq\nr  s&#x9;&#10;'/>");
        assert_eq!(e.attribute("v"), Some("p q r  s\t\n"));
    }

    #[test]
    fn carriage_returns_normalise_to_newlines() {
        let e = root("<a v='x\r\ny\rz'>p\r\nq\rr</a>");
        // In the attribute the newlines then normalise to spaces.
        assert_eq!(e.attribute("v"), Some("x y z"));
        assert_eq!(text_of(&e), "p\nq\nr");
    }

    #[test]
    fn prolog_and_trailing_misc_are_skipped() {
        let e = root(
            "\u{feff}<?xml version='1.0'?>\n<!-- c --><!DOCTYPE a SYSTEM \"a>b\">\n<a/>\n<!-- after -->\n",
        );
        assert_eq!(e.name.local, "a");
        assert!(parse("<a/><b/>").is_err());
    }

    #[test]
    fn mismatched_end_tag_reports_both_positions() {
        let err = parse("<a>\n<b>\n</c>\n</a>").unwrap_err();
        assert_eq!((err.line, err.column), (3, 3));
        assert!(
            err.message
                .contains("closes `<b>` opened at line 2, column 1"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_attributes_are_rejected() {
        assert!(parse("<a x='1' x='2'/>").is_err());
        // Same expanded name reached through two prefixes.
        assert!(parse("<a xmlns:p='u' xmlns:q='u' p:x='1' q:x='2'/>").is_err());
        assert!(parse("<a xmlns:p='u' xmlns:q='v' p:x='1' q:x='2'/>").is_ok());
    }

    #[test]
    fn deep_nesting_errors_rather_than_overflowing_the_stack() {
        let deep = "<a>".repeat(50_000);
        let err = parse(&deep).unwrap_err();
        assert!(err.message.contains("nested more than"), "{err}");
    }

    #[test]
    fn malformed_corpus_produces_errors_not_panics() {
        for doc in [
            "",
            "<",
            "<a",
            "<a>",
            "</a>",
            "<a></b>",
            "<a><b></a></b>",
            "<>",
            "<a/",
            "<a x/>",
            "<a x=/>",
            "<a x='1/>",
            "<a x=1/>",
            "<a x='<'/>",
            "<a x='1'y='2'/>",
            "<a:/>",
            "<:a/>",
            "<a::b/>",
            "<a xmlns:xmlns='u'/>",
            "<a xmlns:p=''/>",
            "<p:a/>",
            "<a>&</a>",
            "<a>&;</a>",
            "<a><![CDATA[</a>",
            "<a><!-- </a>",
            "<a><?pi </a>",
            "<!DOCTYPE a",
            "<!DOCTYPE a [",
            "<?xml",
            "text",
            "<a>text",
            "\u{feff}",
            "<a>\u{0}</a>",
        ] {
            let err = parse(doc).unwrap_err();
            assert!(err.line >= 1 && err.column >= 1, "{doc:?} gave {err}");
        }
    }

    /// The parser's only invariant under garbage input: it returns, and an error
    /// always carries a usable position.
    fn survives(input: &str) -> bool {
        match parse(input) {
            Ok(root) => !root.name.local.is_empty(),
            Err(e) => e.line >= 1 && e.column >= 1,
        }
    }

    const SAMPLE: &str = "<?xml version='1.0'?><svg xmlns='http://www.w3.org/2000/svg' \
        xmlns:x='u'><g transform='translate(1 2)'><!-- c --><path d='M0 0L1 1' \
        fill='&amp;'/><text x:a='&#65;'><![CDATA[<raw>]]>hi</text></g></svg>";

    #[test]
    fn fuzz_token_soup() {
        // Random bytes almost never reach past the first tag, so most of the
        // budget goes on fragments that are individually well-formed and
        // jointly nonsense -- which is where a parser actually breaks.
        fn generate(rng: &mut Rng) -> String {
            const TOKENS: &[&str] = &[
                "<a",
                "<a:b",
                "<b",
                " x='1'",
                " x:y=\"2\"",
                " xmlns='u'",
                " xmlns:a='v'",
                " xmlns=''",
                " xmlns:a=''",
                ">",
                "/>",
                "</a>",
                "</a:b>",
                "</b>",
                "&amp;",
                "&#38;",
                "&#x26;",
                "&bogus;",
                "&#;",
                "&",
                ";",
                "<![CDATA[",
                "]]>",
                "<!--",
                "-->",
                "<?pi ?>",
                "<!DOCTYPE a [<!ENTITY e 'x'>]>",
                "&e;",
                "text",
                "\t",
                "\r\n",
                "  ",
                "\"",
                "'",
                "=",
                "<",
                ">",
                "/",
                ":",
                "\u{feff}",
                "é",
                "\u{1f600}",
            ];
            let count = rng.below(32) + 1;
            (0..count)
                .map(|_| TOKENS[rng.below(TOKENS.len() as u64) as usize])
                .collect()
        }
        check("xml token soup", 20_000, generate, |s| survives(s));
    }

    #[test]
    fn fuzz_truncated_and_spliced() {
        // Every prefix of a valid document is a plausible truncated file, and
        // the cut lands mid-tag, mid-entity and mid-CDATA in turn.
        fn generate(rng: &mut Rng) -> String {
            let cut = rng.below(SAMPLE.len() as u64 + 1) as usize;
            let cut = (0..=cut)
                .rev()
                .find(|i| SAMPLE.is_char_boundary(*i))
                .unwrap_or(0);
            let mut s = SAMPLE[..cut].to_string();
            // Half the cases also get a stray byte, which turns "unfinished" into
            // "wrong" without making it unrecognisable.
            if rng.below(2) == 0 {
                s.push(*b"<>/&;='\"\t\0]".get(rng.below(11) as usize).unwrap() as char);
            }
            s
        }
        check("xml truncation", 20_000, generate, |s| survives(s));
    }

    #[test]
    fn fuzz_random_bytes() {
        fn generate(rng: &mut Rng) -> String {
            let len = rng.below(48) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        check("xml random bytes", 20_000, generate, |s| survives(s));
    }
}

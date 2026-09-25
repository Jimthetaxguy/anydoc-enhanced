//! Stylesheet evaluation for the EPUB hidden-content check.
//!
//! A chapter's text is flagged when a reading system would hide it and
//! AnyDoc would still convert it. Two models answer those questions.
//!
//! - The reader model follows CSS as reading systems apply it: the CSS
//!   Syntax 3 tokenizer (comments, escapes, strings), media queries, the
//!   selector grammar, the cascade, and the user-agent rules that hide
//!   content. Where it cannot decide (a sibling combinator, an unknown
//!   pseudo-class, a value set through `var()`), it lets a hiding rule apply
//!   and keeps a showing rule from overriding one, so it errs toward finding
//!   hidden text.
//! - The AnyDoc model ports AnyDoc 0.2.4's own subset (`shared::html`):
//!   `display` from bare `tag`, `.class`, and `tag.class` rules and from
//!   inline styles, applied only to the elements its walker styles.
//!
//! Text that both models hide, or that AnyDoc drops, is not flagged: the
//! conversion then matches what a reader shows.

use std::collections::HashMap;
use std::rc::Rc;

use super::DocumentError;

/// Selectors in one rule's list, compound selectors in one complex
/// selector, and nested selector lists (`:not(:is(…))`) evaluated before a
/// selector counts as undecidable.
const MAX_SELECTORS_PER_RULE: usize = 256;
const MAX_COMPOUNDS: usize = 32;
const MAX_SELECTOR_NESTING: usize = 8;
/// `@import` statements one stylesheet may carry.
pub(super) const MAX_IMPORTS_PER_SHEET: usize = 256;
/// Style rules that set `display`, `visibility`, or `content-visibility`,
/// across a package's stylesheets. Real books carry a few dozen.
pub(super) const MAX_STYLE_RULES: usize = 16_384;
/// Compound-selector evaluations across a package: one element tested
/// against one rule costs one per compound it reaches.
pub(super) const MAX_MATCH_WORK: u64 = 50_000_000;
/// Nesting depth of a chapter's elements, as AnyDoc's parser allows.
const MAX_CHAPTER_DEPTH: usize = 256;
/// Tokens one stylesheet or attribute may hold. A large real stylesheet has
/// tens of thousands; the cap bounds the memory a crafted one takes.
const MAX_STYLESHEET_TOKENS: usize = 500_000;

// ---------------------------------------------------------------------------
// Tokenizer (CSS Syntax Level 3, section 4)

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Function(String),
    AtKeyword(String),
    Hash(String),
    Str(String),
    Url(String),
    BadUrl,
    /// A number, percentage, or dimension, as written.
    Numeric(String),
    Whitespace,
    Delim(char),
    Colon,
    Semicolon,
    Comma,
    OpenSquare,
    CloseSquare,
    OpenParen,
    CloseParen,
    OpenCurly,
    CloseCurly,
    Cdo,
    Cdc,
}

fn tokenize(css: &str) -> Result<Vec<Token>, DocumentError> {
    let mut tokenizer = Tokenizer::new(css);
    let mut tokens = Vec::new();
    while let Some(token) = tokenizer.next_token() {
        if tokens.len() >= MAX_STYLESHEET_TOKENS {
            return Err(DocumentError::ResourceLimit);
        }
        tokens.push(token);
    }
    Ok(tokens)
}

/// Preprocess CSS text (newlines normalized, NUL replaced) into characters.
fn preprocess(css: &str) -> Vec<char> {
    let mut chars = Vec::with_capacity(css.len());
    let mut input = css.chars().peekable();
    while let Some(character) = input.next() {
        match character {
            '\r' => {
                if input.peek() == Some(&'\n') {
                    input.next();
                }
                chars.push('\n');
            }
            '\u{c}' => chars.push('\n'),
            '\0' => chars.push('\u{fffd}'),
            character => chars.push(character),
        }
    }
    chars
}

struct Tokenizer {
    chars: Vec<char>,
    position: usize,
}

fn is_name_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_' || !character.is_ascii()
}

fn is_name(character: char) -> bool {
    is_name_start(character) || character.is_ascii_digit() || character == '-'
}

fn is_css_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n')
}

impl Tokenizer {
    fn new(css: &str) -> Self {
        Tokenizer {
            chars: preprocess(css),
            position: 0,
        }
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.position + offset).copied()
    }

    fn valid_escape_at(&self, offset: usize) -> bool {
        self.peek(offset) == Some('\\') && self.peek(offset + 1).is_some_and(|next| next != '\n')
    }

    fn starts_identifier_at(&self, offset: usize) -> bool {
        match self.peek(offset) {
            Some('-') => {
                self.peek(offset + 1)
                    .is_some_and(|next| is_name_start(next) || next == '-')
                    || self.valid_escape_at(offset + 1)
            }
            Some('\\') => self.valid_escape_at(offset),
            Some(character) => is_name_start(character),
            None => false,
        }
    }

    fn starts_number_at(&self, offset: usize) -> bool {
        match self.peek(offset) {
            Some('+' | '-') => {
                self.peek(offset + 1)
                    .is_some_and(|next| next.is_ascii_digit())
                    || (self.peek(offset + 1) == Some('.')
                        && self
                            .peek(offset + 2)
                            .is_some_and(|next| next.is_ascii_digit()))
            }
            Some('.') => self
                .peek(offset + 1)
                .is_some_and(|next| next.is_ascii_digit()),
            Some(character) => character.is_ascii_digit(),
            None => false,
        }
    }

    /// Consume an escape whose backslash was already consumed.
    fn consume_escape(&mut self) -> char {
        let Some(character) = self.peek(0) else {
            return '\u{fffd}';
        };
        if character.is_ascii_hexdigit() {
            let mut value = 0u32;
            let mut digits = 0;
            while digits < 6 {
                match self.peek(0).and_then(|digit| digit.to_digit(16)) {
                    Some(digit) => {
                        value = value * 16 + digit;
                        self.position += 1;
                        digits += 1;
                    }
                    None => break,
                }
            }
            if self.peek(0).is_some_and(is_css_whitespace) {
                self.position += 1;
            }
            return match value {
                0 => '\u{fffd}',
                value => char::from_u32(value).unwrap_or('\u{fffd}'),
            };
        }
        self.position += 1;
        character
    }

    fn consume_name(&mut self) -> String {
        let mut name = String::new();
        loop {
            match self.peek(0) {
                Some(character) if is_name(character) => {
                    name.push(character);
                    self.position += 1;
                }
                Some('\\') if self.valid_escape_at(0) => {
                    self.position += 1;
                    name.push(self.consume_escape());
                }
                _ => return name,
            }
        }
    }

    fn consume_string(&mut self, quote: char) -> Token {
        let mut value = String::new();
        loop {
            match self.peek(0) {
                None => return Token::Str(value),
                Some(character) if character == quote => {
                    self.position += 1;
                    return Token::Str(value);
                }
                // A bad string: kept as far as it reached.
                Some('\n') => return Token::Str(value),
                Some('\\') => {
                    self.position += 1;
                    match self.peek(0) {
                        None => {}
                        Some('\n') => self.position += 1,
                        Some(_) => value.push(self.consume_escape()),
                    }
                }
                Some(character) => {
                    value.push(character);
                    self.position += 1;
                }
            }
        }
    }

    fn consume_url(&mut self) -> Token {
        while self.peek(0).is_some_and(is_css_whitespace) {
            self.position += 1;
        }
        let mut value = String::new();
        loop {
            match self.peek(0) {
                None => return Token::Url(value),
                Some(')') => {
                    self.position += 1;
                    return Token::Url(value);
                }
                Some(character) if is_css_whitespace(character) => {
                    while self.peek(0).is_some_and(is_css_whitespace) {
                        self.position += 1;
                    }
                    if matches!(self.peek(0), None | Some(')')) {
                        self.position = (self.position + 1).min(self.chars.len());
                        return Token::Url(value);
                    }
                    self.consume_bad_url();
                    return Token::BadUrl;
                }
                Some('"' | '\'' | '(') => {
                    self.consume_bad_url();
                    return Token::BadUrl;
                }
                Some('\\') => {
                    if self.valid_escape_at(0) {
                        self.position += 1;
                        value.push(self.consume_escape());
                    } else {
                        self.consume_bad_url();
                        return Token::BadUrl;
                    }
                }
                Some(character) => {
                    value.push(character);
                    self.position += 1;
                }
            }
        }
    }

    fn consume_bad_url(&mut self) {
        loop {
            match self.peek(0) {
                None => return,
                Some(')') => {
                    self.position += 1;
                    return;
                }
                Some('\\') if self.valid_escape_at(0) => {
                    self.position += 1;
                    self.consume_escape();
                }
                Some(_) => self.position += 1,
            }
        }
    }

    fn consume_numeric(&mut self) -> Token {
        let start = self.position;
        if matches!(self.peek(0), Some('+' | '-')) {
            self.position += 1;
        }
        while self.peek(0).is_some_and(|digit| digit.is_ascii_digit()) {
            self.position += 1;
        }
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|digit| digit.is_ascii_digit()) {
            self.position += 1;
            while self.peek(0).is_some_and(|digit| digit.is_ascii_digit()) {
                self.position += 1;
            }
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            let signed = matches!(self.peek(1), Some('+' | '-'));
            let digit_at = if signed { 2 } else { 1 };
            if self
                .peek(digit_at)
                .is_some_and(|digit| digit.is_ascii_digit())
            {
                self.position += digit_at;
                while self.peek(0).is_some_and(|digit| digit.is_ascii_digit()) {
                    self.position += 1;
                }
            }
        }
        let mut text: String = self.chars[start..self.position].iter().collect();
        if self.starts_identifier_at(0) {
            text.push_str(&self.consume_name());
        } else if self.peek(0) == Some('%') {
            self.position += 1;
            text.push('%');
        }
        Token::Numeric(text)
    }

    fn consume_ident_like(&mut self) -> Token {
        let name = self.consume_name();
        if self.peek(0) == Some('(') {
            self.position += 1;
            if name.eq_ignore_ascii_case("url") {
                let mut offset = 0;
                while self.peek(offset).is_some_and(is_css_whitespace) {
                    offset += 1;
                }
                if matches!(self.peek(offset), Some('"' | '\'')) {
                    return Token::Function(name);
                }
                return self.consume_url();
            }
            return Token::Function(name);
        }
        Token::Ident(name)
    }

    fn next_token(&mut self) -> Option<Token> {
        while self.peek(0) == Some('/') && self.peek(1) == Some('*') {
            self.position += 2;
            while self.position < self.chars.len()
                && !(self.peek(0) == Some('*') && self.peek(1) == Some('/'))
            {
                self.position += 1;
            }
            self.position = (self.position + 2).min(self.chars.len());
        }
        let character = self.peek(0)?;
        if is_css_whitespace(character) {
            while self.peek(0).is_some_and(is_css_whitespace) {
                self.position += 1;
            }
            return Some(Token::Whitespace);
        }
        let token = match character {
            '"' | '\'' => {
                self.position += 1;
                self.consume_string(character)
            }
            '#' if self.peek(1).is_some_and(is_name) || self.valid_escape_at(1) => {
                self.position += 1;
                Token::Hash(self.consume_name())
            }
            '(' => self.single(Token::OpenParen),
            ')' => self.single(Token::CloseParen),
            '[' => self.single(Token::OpenSquare),
            ']' => self.single(Token::CloseSquare),
            '{' => self.single(Token::OpenCurly),
            '}' => self.single(Token::CloseCurly),
            ',' => self.single(Token::Comma),
            ':' => self.single(Token::Colon),
            ';' => self.single(Token::Semicolon),
            '+' | '.' if self.starts_number_at(0) => self.consume_numeric(),
            '-' if self.starts_number_at(0) => self.consume_numeric(),
            '-' if self.peek(1) == Some('-') && self.peek(2) == Some('>') => {
                self.position += 3;
                Token::Cdc
            }
            '-' if self.starts_identifier_at(0) => self.consume_ident_like(),
            '<' if self.peek(1) == Some('!')
                && self.peek(2) == Some('-')
                && self.peek(3) == Some('-') =>
            {
                self.position += 4;
                Token::Cdo
            }
            '@' if self.starts_identifier_at(1) => {
                self.position += 1;
                Token::AtKeyword(self.consume_name())
            }
            '\\' if self.valid_escape_at(0) => self.consume_ident_like(),
            character if character.is_ascii_digit() => self.consume_numeric(),
            character if is_name_start(character) => self.consume_ident_like(),
            character => self.single(Token::Delim(character)),
        };
        Some(token)
    }

    fn single(&mut self, token: Token) -> Token {
        self.position += 1;
        token
    }
}

/// Where the block or function opening at `start` ends: the index past its
/// matching closer, and whether it has one. A mismatched closer is an
/// ordinary token, and an unclosed block runs to the end of the input.
fn block_span(tokens: &[Token], start: usize) -> (usize, bool) {
    let mut closers: Vec<char> = Vec::new();
    for (index, token) in tokens.iter().enumerate().skip(start) {
        let closer = match token {
            Token::OpenParen | Token::Function(_) => {
                closers.push(')');
                continue;
            }
            Token::OpenSquare => {
                closers.push(']');
                continue;
            }
            Token::OpenCurly => {
                closers.push('}');
                continue;
            }
            Token::CloseParen => ')',
            Token::CloseSquare => ']',
            Token::CloseCurly => '}',
            _ => continue,
        };
        if closers.last() == Some(&closer) {
            closers.pop();
            if closers.is_empty() {
                return (index + 1, true);
            }
        }
    }
    (tokens.len(), false)
}

fn opens_block(token: &Token) -> bool {
    matches!(
        token,
        Token::OpenParen | Token::OpenSquare | Token::OpenCurly | Token::Function(_)
    )
}

/// The block opening at `start`: its contents, and the index past it.
fn block_at(tokens: &[Token], start: usize) -> (&[Token], usize) {
    let (end, closed) = block_span(tokens, start);
    let inner_end = if closed { end - 1 } else { end };
    (&tokens[start + 1..inner_end.max(start + 1)], end)
}

/// The index past the component value at `index`: a whole block or
/// function, or one token.
fn skip_component(tokens: &[Token], index: usize) -> usize {
    if opens_block(&tokens[index]) {
        block_span(tokens, index).0
    } else {
        index + 1
    }
}

fn trim_whitespace(mut tokens: &[Token]) -> &[Token] {
    while let [Token::Whitespace, rest @ ..] = tokens {
        tokens = rest;
    }
    while let [rest @ .., Token::Whitespace] = tokens {
        tokens = rest;
    }
    tokens
}

/// Split a token list at top-level occurrences of `separator`.
fn split_top_level<'a>(tokens: &'a [Token], separator: &Token) -> Vec<&'a [Token]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < tokens.len() {
        if &tokens[index] == separator {
            parts.push(&tokens[start..index]);
            index += 1;
            start = index;
        } else {
            index = skip_component(tokens, index);
        }
    }
    parts.push(&tokens[start..]);
    parts
}

// ---------------------------------------------------------------------------
// Media queries

/// Whether a media query list can apply on a reading system's screen. An
/// empty list applies. A query applies when its media type is `all`,
/// `screen`, or absent; feature conditions are assumed to hold. `print`,
/// `speech`, the deprecated types, and unknown types such as `amzn-mobi`
/// never match on a screen. A negated query applies unless it negates only
/// `all` or `screen`.
fn media_applies(tokens: &[Token]) -> bool {
    let tokens = trim_whitespace(tokens);
    if tokens.is_empty() {
        return true;
    }
    split_top_level(tokens, &Token::Comma)
        .into_iter()
        .any(|query| {
            let query = trim_whitespace(query);
            let mut words = Vec::new();
            let mut conditions = false;
            let mut index = 0;
            while index < query.len() {
                match &query[index] {
                    Token::Ident(word) => {
                        let word = word.to_ascii_lowercase();
                        if !matches!(word.as_str(), "and" | "or" | "only") {
                            words.push(word);
                        }
                    }
                    Token::Whitespace => {}
                    _ => conditions = true,
                }
                index = skip_component(query, index);
            }
            match words.as_slice() {
                [not, rest @ ..] if not == "not" => {
                    conditions || !matches!(rest, [kind] if kind == "all" || kind == "screen")
                }
                [] => true,
                [kind, ..] => kind == "all" || kind == "screen",
            }
        })
}

/// Whether a `media` attribute or pseudo-attribute can apply on a screen.
pub(super) fn media_attribute_applies(value: &str) -> bool {
    // A media list too long to read counts as applying.
    match tokenize(value) {
        Ok(tokens) => media_applies(&tokens),
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Declarations

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Property {
    Display,
    Visibility,
    ContentVisibility,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    Hide,
    Show,
    /// Takes the parent's value (`visibility: inherit`).
    Inherit,
    /// A value a reader ignores, such as an invalid keyword.
    Neutral,
}

#[derive(Clone, Copy, Debug)]
struct Declaration {
    property: Property,
    effect: Effect,
    important: bool,
}

const DISPLAY_KEYWORDS: [&str; 44] = [
    "block",
    "inline",
    "run-in",
    "flow",
    "flow-root",
    "table",
    "flex",
    "grid",
    "ruby",
    "list-item",
    "contents",
    "inline-block",
    "inline-table",
    "inline-flex",
    "inline-grid",
    "inline-list-item",
    "table-row-group",
    "table-header-group",
    "table-footer-group",
    "table-row",
    "table-cell",
    "table-column-group",
    "table-column",
    "table-caption",
    "ruby-base",
    "ruby-text",
    "ruby-base-container",
    "ruby-text-container",
    "math",
    "compact",
    "marker",
    "-webkit-box",
    "-webkit-inline-box",
    "-webkit-flex",
    "-webkit-inline-flex",
    "-moz-box",
    "-moz-inline-box",
    "-ms-flexbox",
    "-ms-inline-flexbox",
    "-ms-grid",
    "-ms-inline-grid",
    "inherit",
    "initial",
    "unset",
];

/// The declaration a token run holds, if it sets `display`, `visibility`,
/// or `content-visibility`. A value computed at run time (`var()`, `env()`,
/// `attr()`, `if()`) may hide, so it counts as hiding.
fn parse_declaration(tokens: &[Token]) -> Option<Declaration> {
    let tokens = trim_whitespace(tokens);
    let [Token::Ident(name), rest @ ..] = tokens else {
        return None;
    };
    let property = match name.to_ascii_lowercase().as_str() {
        "display" => Property::Display,
        "visibility" => Property::Visibility,
        "content-visibility" => Property::ContentVisibility,
        _ => return None,
    };
    let [Token::Colon, value @ ..] = trim_whitespace(rest) else {
        return None;
    };
    let mut value = trim_whitespace(value);
    let mut important = false;
    if let [before @ .., Token::Ident(word)] = value {
        if word.eq_ignore_ascii_case("important") {
            if let [before @ .., Token::Delim('!')] = trim_whitespace(before) {
                important = true;
                value = trim_whitespace(before);
            }
        }
    }
    let mut keywords = Vec::new();
    let mut computed = false;
    let mut other = false;
    for token in value {
        match token {
            Token::Ident(word) => keywords.push(word.to_ascii_lowercase()),
            Token::Function(function)
                if ["var", "env", "attr", "if"]
                    .iter()
                    .any(|name| function.eq_ignore_ascii_case(name)) =>
            {
                computed = true;
            }
            Token::Whitespace | Token::CloseParen => {}
            _ => other = true,
        }
    }
    let has = |wanted: &[&str]| keywords.iter().any(|word| wanted.contains(&word.as_str()));
    let effect = match property {
        Property::Display if computed || has(&["none"]) => Effect::Hide,
        Property::Display
            if !other
                && !keywords.is_empty()
                && keywords
                    .iter()
                    .all(|word| DISPLAY_KEYWORDS.contains(&word.as_str())) =>
        {
            Effect::Show
        }
        Property::Visibility if computed || has(&["hidden", "collapse"]) => Effect::Hide,
        Property::Visibility if keywords == ["visible"] || keywords == ["initial"] => Effect::Show,
        Property::Visibility if has(&["inherit", "unset"]) && keywords.len() == 1 => {
            Effect::Inherit
        }
        Property::ContentVisibility if computed || has(&["hidden"]) => Effect::Hide,
        Property::ContentVisibility
            if keywords.len() == 1 && has(&["visible", "auto", "initial", "unset"]) =>
        {
            Effect::Show
        }
        _ => Effect::Neutral,
    };
    Some(Declaration {
        property,
        effect,
        important,
    })
}

/// The declarations of an inline `style` attribute.
fn inline_declarations(style: &str) -> Result<Vec<Declaration>, DocumentError> {
    let tokens = tokenize(style)?;
    Ok(split_top_level(&tokens, &Token::Semicolon)
        .into_iter()
        .filter_map(parse_declaration)
        .collect())
}

// ---------------------------------------------------------------------------
// Selectors

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tri {
    No,
    Maybe,
    Yes,
}

impl Tri {
    fn not(self) -> Self {
        match self {
            Tri::No => Tri::Yes,
            Tri::Maybe => Tri::Maybe,
            Tri::Yes => Tri::No,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Combinator {
    Descendant,
    Child,
    /// `+` or `~`: siblings are not tracked, so the step is undecided.
    Sibling,
}

#[derive(Clone, Copy, Debug)]
enum AttributeOperator {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Clone, Debug)]
enum Simple {
    /// A type selector, or the universal one (`None`); `namespaced` when a
    /// namespace prefix other than `*` narrows it.
    Type {
        name: Option<String>,
        namespaced: bool,
    },
    Id(String),
    Class(String),
    Attribute {
        name: String,
        namespaced: bool,
        operator: AttributeOperator,
        value: String,
        case_insensitive: bool,
    },
    Pseudo(PseudoClass),
    /// Syntax this model does not interpret; it may match.
    Unknown,
}

#[derive(Clone, Debug)]
enum PseudoClass {
    /// False in a document at rest: `:hover`, `:focus`, `:target`.
    Never,
    Always,
    Link,
    Root,
    FirstChild,
    NthChild {
        step: i64,
        offset: i64,
    },
    Not(Vec<ComplexSelector>),
    Is(Vec<ComplexSelector>),
    Undecided,
}

#[derive(Clone, Debug, Default)]
struct Compound {
    parts: Vec<Simple>,
}

#[derive(Clone, Debug)]
struct ComplexSelector {
    /// Left to right; `combinators[i]` joins `compounds[i]` and
    /// `compounds[i + 1]`.
    compounds: Vec<Compound>,
    combinators: Vec<Combinator>,
    specificity: (u32, u32, u32),
    /// Styles a pseudo-element, whose hiding hides no element text.
    pseudo_element: bool,
}

impl ComplexSelector {
    /// A selector that may match any element.
    fn undecided() -> Self {
        ComplexSelector {
            compounds: vec![Compound {
                parts: vec![Simple::Unknown],
            }],
            combinators: Vec::new(),
            specificity: (0, 0, 0),
            pseudo_element: false,
        }
    }
}

fn add_specificity(total: &mut (u32, u32, u32), other: (u32, u32, u32)) {
    total.0 += other.0;
    total.1 += other.1;
    total.2 += other.2;
}

fn parse_selector_list(tokens: &[Token], nesting: usize) -> Vec<ComplexSelector> {
    let parts = split_top_level(tokens, &Token::Comma);
    if parts.len() > MAX_SELECTORS_PER_RULE {
        return vec![ComplexSelector::undecided()];
    }
    parts
        .into_iter()
        .map(|part| parse_complex(trim_whitespace(part), nesting))
        .collect()
}

enum SelectorItem {
    Compound(Compound),
    Combinator(Combinator),
}

fn parse_complex(tokens: &[Token], nesting: usize) -> ComplexSelector {
    let mut items: Vec<SelectorItem> = Vec::new();
    let mut current = Compound::default();
    let mut specificity = (0, 0, 0);
    let mut pseudo_element = false;
    let mut index = 0;
    let flush = |current: &mut Compound, items: &mut Vec<SelectorItem>| {
        if !current.parts.is_empty() {
            items.push(SelectorItem::Compound(std::mem::take(current)));
        }
    };
    while index < tokens.len() {
        let combinator = match &tokens[index] {
            Token::Whitespace => Some(Combinator::Descendant),
            Token::Delim('>') => Some(Combinator::Child),
            Token::Delim('+' | '~') => Some(Combinator::Sibling),
            Token::Delim('|') if tokens.get(index + 1) == Some(&Token::Delim('|')) => {
                index += 1;
                Some(Combinator::Descendant)
            }
            _ => None,
        };
        if let Some(combinator) = combinator {
            flush(&mut current, &mut items);
            match items.last_mut() {
                Some(SelectorItem::Compound(_)) => items.push(SelectorItem::Combinator(combinator)),
                Some(SelectorItem::Combinator(previous)) => {
                    if combinator != Combinator::Descendant {
                        *previous = combinator;
                    }
                }
                // A leading combinator (a relative selector) constrains
                // nothing this model reads.
                None => {}
            }
            index += 1;
            continue;
        }
        index = parse_simple(
            tokens,
            index,
            nesting,
            &mut current,
            &mut specificity,
            &mut pseudo_element,
        );
    }
    flush(&mut current, &mut items);
    while matches!(items.last(), Some(SelectorItem::Combinator(_))) {
        items.pop();
    }
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    for item in items {
        match item {
            SelectorItem::Compound(compound) => compounds.push(compound),
            SelectorItem::Combinator(combinator) => combinators.push(combinator),
        }
    }
    if compounds.is_empty() || compounds.len() > MAX_COMPOUNDS {
        let mut undecided = ComplexSelector::undecided();
        undecided.pseudo_element = pseudo_element;
        return undecided;
    }
    ComplexSelector {
        compounds,
        combinators,
        specificity,
        pseudo_element,
    }
}

/// Parse the simple selector at `index` into `compound`; return the index
/// past it.
fn parse_simple(
    tokens: &[Token],
    index: usize,
    nesting: usize,
    compound: &mut Compound,
    specificity: &mut (u32, u32, u32),
    pseudo_element: &mut bool,
) -> usize {
    let name_at = |at: usize| matches!(tokens.get(at), Some(Token::Ident(_) | Token::Delim('*')));
    match &tokens[index] {
        Token::Ident(_) | Token::Delim('*') | Token::Delim('|') => {
            // E, *, ns|E, ns|*, *|E, *|*, |E, |*
            let (namespaced, name_index) = if tokens[index] == Token::Delim('|') {
                if !name_at(index + 1) {
                    compound.parts.push(Simple::Unknown);
                    return index + 1;
                }
                (true, index + 1)
            } else if tokens.get(index + 1) == Some(&Token::Delim('|')) && name_at(index + 2) {
                (tokens[index] != Token::Delim('*'), index + 2)
            } else {
                (false, index)
            };
            let name = match &tokens[name_index] {
                Token::Ident(name) => {
                    specificity.2 += 1;
                    Some(name.to_ascii_lowercase())
                }
                _ => None,
            };
            compound.parts.push(Simple::Type { name, namespaced });
            name_index + 1
        }
        Token::Hash(id) => {
            specificity.0 += 1;
            compound.parts.push(Simple::Id(id.clone()));
            index + 1
        }
        Token::Delim('.') => match tokens.get(index + 1) {
            Some(Token::Ident(class)) => {
                specificity.1 += 1;
                compound.parts.push(Simple::Class(class.clone()));
                index + 2
            }
            _ => {
                compound.parts.push(Simple::Unknown);
                index + 1
            }
        },
        Token::OpenSquare => {
            let (contents, end) = block_at(tokens, index);
            specificity.1 += 1;
            compound.parts.push(parse_attribute(contents));
            end
        }
        Token::Colon => {
            if tokens.get(index + 1) == Some(&Token::Colon) {
                *pseudo_element = true;
                specificity.2 += 1;
                return if index + 2 < tokens.len() {
                    skip_component(tokens, index + 2)
                } else {
                    index + 2
                };
            }
            match tokens.get(index + 1) {
                Some(Token::Ident(name)) => {
                    let name = name.to_ascii_lowercase();
                    if matches!(
                        name.as_str(),
                        "before" | "after" | "first-line" | "first-letter"
                    ) {
                        *pseudo_element = true;
                        specificity.2 += 1;
                    } else {
                        specificity.1 += 1;
                        compound.parts.push(Simple::Pseudo(pseudo_class(&name)));
                    }
                    index + 2
                }
                Some(Token::Function(name)) => {
                    let (arguments, end) = block_at(tokens, index + 1);
                    let pseudo = functional_pseudo_class(name, arguments, nesting, specificity);
                    compound.parts.push(Simple::Pseudo(pseudo));
                    end
                }
                _ => {
                    compound.parts.push(Simple::Unknown);
                    index + 1
                }
            }
        }
        _ => {
            compound.parts.push(Simple::Unknown);
            skip_component(tokens, index)
        }
    }
}

fn pseudo_class(name: &str) -> PseudoClass {
    match name {
        "hover" | "active" | "focus" | "focus-visible" | "focus-within" | "target"
        | "target-within" | "visited" | "current" | "past" | "future" | "playing" | "paused"
        | "seeking" | "buffering" | "stalled" | "muted" | "volume-locked" | "fullscreen"
        | "picture-in-picture" | "user-invalid" | "user-valid" | "autofill"
        | "-webkit-autofill" | "modal" | "popover-open" | "host" => PseudoClass::Never,
        "link" | "any-link" | "-webkit-any-link" => PseudoClass::Link,
        "root" | "scope" => PseudoClass::Root,
        "first-child" => PseudoClass::FirstChild,
        "defined" => PseudoClass::Always,
        _ => PseudoClass::Undecided,
    }
}

fn functional_pseudo_class(
    name: &str,
    arguments: &[Token],
    nesting: usize,
    specificity: &mut (u32, u32, u32),
) -> PseudoClass {
    let name = name.to_ascii_lowercase();
    let list = |specificity: &mut (u32, u32, u32), counts: bool| {
        let list = parse_selector_list(arguments, nesting + 1);
        if counts {
            let highest = list
                .iter()
                .map(|selector| selector.specificity)
                .max()
                .unwrap_or((0, 0, 0));
            add_specificity(specificity, highest);
        }
        list
    };
    match name.as_str() {
        _ if nesting >= MAX_SELECTOR_NESTING => {
            specificity.1 += 1;
            PseudoClass::Undecided
        }
        "not" => PseudoClass::Not(list(specificity, true)),
        "is" | "matches" | "-webkit-any" | "-moz-any" => PseudoClass::Is(list(specificity, true)),
        "where" => PseudoClass::Is(list(specificity, false)),
        "has" => {
            list(specificity, true);
            PseudoClass::Undecided
        }
        "host" | "host-context" => PseudoClass::Never,
        "nth-child" => {
            specificity.1 += 1;
            parse_nth(arguments).map_or(PseudoClass::Undecided, |(step, offset)| {
                PseudoClass::NthChild { step, offset }
            })
        }
        _ => {
            specificity.1 += 1;
            PseudoClass::Undecided
        }
    }
}

/// An `An+B` argument, or `None` for anything else (such as `of S`).
fn parse_nth(tokens: &[Token]) -> Option<(i64, i64)> {
    let mut text = String::new();
    for token in tokens {
        match token {
            Token::Whitespace => {}
            Token::Ident(value) | Token::Numeric(value) => text.push_str(value),
            Token::Delim(character) => text.push(*character),
            _ => return None,
        }
    }
    let text = text.to_ascii_lowercase();
    match text.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    match text.split_once('n') {
        None => text.parse().ok().map(|offset| (0, offset)),
        Some((step, offset)) => {
            let step = match step {
                "" | "+" => 1,
                "-" => -1,
                step => step.parse().ok()?,
            };
            let offset = if offset.is_empty() {
                0
            } else {
                offset.parse().ok()?
            };
            Some((step, offset))
        }
    }
}

fn parse_attribute(tokens: &[Token]) -> Simple {
    let tokens = trim_whitespace(tokens);
    let (name, namespaced, rest) = match tokens {
        [Token::Ident(_), Token::Delim('|'), Token::Ident(name), rest @ ..] => (name, true, rest),
        [Token::Delim('*'), Token::Delim('|'), Token::Ident(name), rest @ ..]
        | [Token::Delim('|'), Token::Ident(name), rest @ ..]
        | [Token::Ident(name), rest @ ..] => (name, false, rest),
        _ => return Simple::Unknown,
    };
    let rest = trim_whitespace(rest);
    let (operator, rest) = match rest {
        [] => {
            return Simple::Attribute {
                name: name.to_ascii_lowercase(),
                namespaced,
                operator: AttributeOperator::Exists,
                value: String::new(),
                case_insensitive: false,
            };
        }
        [Token::Delim('='), rest @ ..] => (AttributeOperator::Equals, rest),
        [Token::Delim('~'), Token::Delim('='), rest @ ..] => (AttributeOperator::Includes, rest),
        [Token::Delim('|'), Token::Delim('='), rest @ ..] => (AttributeOperator::DashMatch, rest),
        [Token::Delim('^'), Token::Delim('='), rest @ ..] => (AttributeOperator::Prefix, rest),
        [Token::Delim('$'), Token::Delim('='), rest @ ..] => (AttributeOperator::Suffix, rest),
        [Token::Delim('*'), Token::Delim('='), rest @ ..] => (AttributeOperator::Substring, rest),
        _ => return Simple::Unknown,
    };
    let (value, rest) = match trim_whitespace(rest) {
        [Token::Ident(value) | Token::Str(value), rest @ ..] => (value.clone(), rest),
        _ => return Simple::Unknown,
    };
    let case_insensitive = match trim_whitespace(rest) {
        [] => false,
        [Token::Ident(flag)] if flag.eq_ignore_ascii_case("i") => true,
        [Token::Ident(flag)] if flag.eq_ignore_ascii_case("s") => false,
        _ => return Simple::Unknown,
    };
    Simple::Attribute {
        name: name.to_ascii_lowercase(),
        namespaced,
        operator,
        value,
        case_insensitive,
    }
}

// ---------------------------------------------------------------------------
// Matching

/// An element as the selector engine sees it.
pub(super) struct Element {
    /// The local name as written.
    local: String,
    lower: String,
    /// Attribute local names (lowercased), whether each is prefixed, and
    /// decoded values, in document order.
    attributes: Vec<(String, bool, String)>,
    /// Position among the parent's element children, from 1.
    index: usize,
}

impl Element {
    pub(super) fn new(local: &str, attributes: Vec<(String, bool, String)>, index: usize) -> Self {
        Element {
            local: local.to_string(),
            lower: local.to_ascii_lowercase(),
            attributes,
            index,
        }
    }

    fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = (bool, &'a str)> + 'a {
        self.attributes
            .iter()
            .filter(move |(attribute, _, _)| attribute == name)
            .map(|(_, prefixed, value)| (*prefixed, value.as_str()))
    }

    /// The first value of an attribute by local name, in any namespace, as
    /// AnyDoc's `attr_any` reads it.
    fn first(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(attribute, _, _)| attribute == name)
            .map(|(_, _, value)| value.as_str())
    }
}

fn attribute_test(operator: AttributeOperator, actual: &str, wanted: &str) -> bool {
    match operator {
        AttributeOperator::Exists => true,
        AttributeOperator::Equals => actual == wanted,
        AttributeOperator::Includes => {
            !wanted.is_empty()
                && !wanted.contains(char::is_whitespace)
                && actual.split_ascii_whitespace().any(|word| word == wanted)
        }
        AttributeOperator::DashMatch => {
            actual == wanted
                || actual
                    .strip_prefix(wanted)
                    .is_some_and(|rest| rest.starts_with('-'))
        }
        AttributeOperator::Prefix => !wanted.is_empty() && actual.starts_with(wanted),
        AttributeOperator::Suffix => !wanted.is_empty() && actual.ends_with(wanted),
        AttributeOperator::Substring => !wanted.is_empty() && actual.contains(wanted),
    }
}

/// How an element's attribute values meet a test: exactly on an unprefixed
/// attribute, or only with a prefix or without regard to case.
fn attribute_certainty(
    element: &Element,
    name: &str,
    test: impl Fn(&str, bool) -> bool,
    exact_prefixed: bool,
) -> Tri {
    let mut result = Tri::No;
    for (prefixed, value) in element.values(name) {
        if test(value, false) {
            if !prefixed || exact_prefixed {
                return Tri::Yes;
            }
            result = Tri::Maybe;
        } else if test(value, true) {
            result = Tri::Maybe;
        }
    }
    result
}

fn nth_matches(step: i64, offset: i64, index: usize) -> bool {
    let index = index as i64;
    if step == 0 {
        return index == offset;
    }
    let distance = index - offset;
    distance % step == 0 && distance / step >= 0
}

fn match_simple(simple: &Simple, stack: &[Element], position: usize, work: &mut u64) -> Tri {
    let element = &stack[position];
    match simple {
        Simple::Type {
            name: None,
            namespaced,
        } => {
            if *namespaced {
                Tri::Maybe
            } else {
                Tri::Yes
            }
        }
        Simple::Type {
            name: Some(name),
            namespaced,
        } => {
            if element.lower != *name {
                Tri::No
            } else if element.local == *name && !namespaced {
                Tri::Yes
            } else {
                Tri::Maybe
            }
        }
        Simple::Id(id) => attribute_certainty(
            element,
            "id",
            |value, loose| {
                if loose {
                    value.eq_ignore_ascii_case(id)
                } else {
                    value == id
                }
            },
            false,
        ),
        Simple::Class(class) => attribute_certainty(
            element,
            "class",
            |value, loose| {
                value.split_ascii_whitespace().any(|word| {
                    if loose {
                        word.eq_ignore_ascii_case(class)
                    } else {
                        word == class
                    }
                })
            },
            false,
        ),
        Simple::Attribute {
            name,
            namespaced,
            operator,
            value,
            case_insensitive,
        } => {
            let certainty = attribute_certainty(
                element,
                name,
                |actual, loose| {
                    if loose || *case_insensitive {
                        attribute_test(*operator, &actual.to_lowercase(), &value.to_lowercase())
                    } else {
                        attribute_test(*operator, actual, value)
                    }
                },
                *namespaced,
            );
            if *namespaced && certainty == Tri::Yes {
                Tri::Maybe
            } else {
                certainty
            }
        }
        Simple::Pseudo(pseudo) => match pseudo {
            PseudoClass::Never => Tri::No,
            PseudoClass::Always => Tri::Yes,
            PseudoClass::Link => {
                if matches!(element.lower.as_str(), "a" | "area" | "link")
                    && element.values("href").next().is_some()
                {
                    Tri::Yes
                } else {
                    Tri::No
                }
            }
            PseudoClass::Root => {
                if position == 0 {
                    Tri::Yes
                } else {
                    Tri::No
                }
            }
            PseudoClass::FirstChild => {
                if element.index == 1 {
                    Tri::Yes
                } else {
                    Tri::No
                }
            }
            PseudoClass::NthChild { step, offset } => {
                if nth_matches(*step, *offset, element.index) {
                    Tri::Yes
                } else {
                    Tri::No
                }
            }
            PseudoClass::Not(list) => list
                .iter()
                .map(|selector| match_complex(selector, &stack[..=position], work))
                .max()
                .unwrap_or(Tri::No)
                .not(),
            PseudoClass::Is(list) => list
                .iter()
                .map(|selector| match_complex(selector, &stack[..=position], work))
                .max()
                .unwrap_or(Tri::No),
            PseudoClass::Undecided => Tri::Maybe,
        },
        Simple::Unknown => Tri::Maybe,
    }
}

fn match_compound(compound: &Compound, stack: &[Element], position: usize, work: &mut u64) -> Tri {
    *work += 1;
    let mut result = Tri::Yes;
    for simple in &compound.parts {
        result = result.min(match_simple(simple, stack, position, work));
        if result == Tri::No {
            break;
        }
    }
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    /// Whether the selector could match: undecided parts count.
    Possible,
    /// Whether it certainly matches.
    Certain,
}

/// Walk a selector right to left from the element at the top of `stack`.
/// A descendant step takes the nearest ancestor that fits; when a later
/// child step then fails, a farther ancestor might have fitted, so a
/// possible match cannot be ruled out.
fn match_chain(
    selector: &ComplexSelector,
    stack: &[Element],
    mode: MatchMode,
    work: &mut u64,
) -> bool {
    let accepts = |tri: Tri| match mode {
        MatchMode::Possible => tri != Tri::No,
        MatchMode::Certain => tri == Tri::Yes,
    };
    let last = selector.compounds.len() - 1;
    let mut position = stack.len() - 1;
    if !accepts(match_compound(
        &selector.compounds[last],
        stack,
        position,
        work,
    )) {
        return false;
    }
    let mut chose_ancestor = false;
    for step in (0..last).rev() {
        let compound = &selector.compounds[step];
        match selector.combinators[step] {
            Combinator::Child => {
                let fits = position > 0 && {
                    position -= 1;
                    accepts(match_compound(compound, stack, position, work))
                };
                if !fits {
                    return mode == MatchMode::Possible && chose_ancestor;
                }
            }
            Combinator::Descendant => {
                let found = (0..position)
                    .rev()
                    .find(|&candidate| accepts(match_compound(compound, stack, candidate, work)));
                match found {
                    Some(candidate) => {
                        position = candidate;
                        chose_ancestor = true;
                    }
                    None => return false,
                }
            }
            Combinator::Sibling => {
                if mode == MatchMode::Certain {
                    return false;
                }
            }
        }
    }
    true
}

fn match_complex(selector: &ComplexSelector, stack: &[Element], work: &mut u64) -> Tri {
    if !match_chain(selector, stack, MatchMode::Possible, work) {
        Tri::No
    } else if match_chain(selector, stack, MatchMode::Certain, work) {
        Tri::Yes
    } else {
        Tri::Maybe
    }
}

// ---------------------------------------------------------------------------
// Stylesheets

/// A style rule that sets a property the check reads.
#[derive(Debug)]
struct StyleRule {
    selector: ComplexSelector,
    declarations: Rc<[Declaration]>,
}

/// A stylesheet reduced to what the check reads: its rules that set
/// `display`, `visibility`, or `content-visibility` where their media
/// apply, and the stylesheets it imports for a screen, in order.
#[derive(Debug, Default)]
pub(super) struct Stylesheet {
    rules: Vec<Rc<StyleRule>>,
    pub(super) imports: Vec<String>,
}

impl Stylesheet {
    pub(super) fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// Nesting of at-rules and nested style rules followed before a sheet is
/// refused as a resource limit.
const MAX_RULE_NESTING: usize = 32;

/// Parse a stylesheet the way a reading system reads it.
pub(super) fn parse_stylesheet(css: &str) -> Result<Stylesheet, DocumentError> {
    let tokens = tokenize(css)?;
    let mut sheet = Stylesheet::default();
    parse_rule_list(&tokens, true, true, &mut sheet, 0)?;
    Ok(sheet)
}

fn parse_rule_list(
    tokens: &[Token],
    top_level: bool,
    media: bool,
    sheet: &mut Stylesheet,
    nesting: usize,
) -> Result<(), DocumentError> {
    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index] {
            Token::Whitespace | Token::Cdo | Token::Cdc | Token::Semicolon => index += 1,
            Token::AtKeyword(name) => {
                index = parse_at_rule(tokens, index, name, top_level, media, None, sheet, nesting)?;
            }
            _ => {
                let start = index;
                while index < tokens.len() && tokens[index] != Token::OpenCurly {
                    index = skip_component(tokens, index);
                }
                if index >= tokens.len() {
                    break;
                }
                let (block, end) = block_at(tokens, index);
                parse_style_block(&tokens[start..index], block, media, sheet, nesting)?;
                index = end;
            }
        }
    }
    Ok(())
}

/// Parse the at-rule at `index`; return the index past it. `selectors` is
/// the enclosing style rule's prelude for an at-rule nested in one, whose
/// declarations style the same elements.
#[allow(clippy::too_many_arguments)]
fn parse_at_rule(
    tokens: &[Token],
    index: usize,
    name: &str,
    top_level: bool,
    media: bool,
    selectors: Option<&[Token]>,
    sheet: &mut Stylesheet,
    nesting: usize,
) -> Result<usize, DocumentError> {
    let name = name.to_ascii_lowercase();
    let mut end = index + 1;
    while end < tokens.len() && !matches!(tokens[end], Token::Semicolon | Token::OpenCurly) {
        end = skip_component(tokens, end);
    }
    let prelude = &tokens[index + 1..end];
    if end < tokens.len() && tokens[end] == Token::OpenCurly {
        let (block, after) = block_at(tokens, end);
        let applies = match name.as_str() {
            "media" => Some(media_applies(prelude)),
            "supports" | "layer" | "container" | "document" | "-moz-document" | "scope"
            | "starting-style" => Some(true),
            // `@font-face`, `@page`, `@keyframes`, and unknown at-rules
            // style no elements.
            _ => None,
        };
        if let Some(applies) = applies {
            if nesting >= MAX_RULE_NESTING {
                return Err(DocumentError::ResourceLimit);
            }
            match selectors {
                Some(selectors) => {
                    parse_style_block(selectors, block, media && applies, sheet, nesting + 1)?;
                }
                None => parse_rule_list(block, false, media && applies, sheet, nesting + 1)?,
            }
        }
        return Ok(after);
    }
    if name == "import" && top_level && selectors.is_none() {
        if let Some((target, applies)) = import_target(prelude) {
            if media && applies {
                if sheet.imports.len() >= MAX_IMPORTS_PER_SHEET {
                    return Err(DocumentError::ResourceLimit);
                }
                sheet.imports.push(target);
            }
        }
    }
    Ok((end + 1).min(tokens.len()))
}

/// An `@import`'s target, and whether its media list applies on a screen.
fn import_target(prelude: &[Token]) -> Option<(String, bool)> {
    let tokens = trim_whitespace(prelude);
    let (target, rest) = match tokens {
        [Token::Str(target) | Token::Url(target), rest @ ..] => (target.clone(), rest),
        [Token::Function(function), ..] if function.eq_ignore_ascii_case("url") => {
            let (arguments, end) = block_at(tokens, 0);
            match trim_whitespace(arguments) {
                [Token::Str(target)] => (target.clone(), &tokens[end..]),
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut rest = trim_whitespace(rest);
    loop {
        match rest {
            [Token::Ident(word), tail @ ..] if word.eq_ignore_ascii_case("layer") => {
                rest = trim_whitespace(tail);
            }
            [Token::Function(function), ..]
                if function.eq_ignore_ascii_case("layer")
                    || function.eq_ignore_ascii_case("supports") =>
            {
                let (_, end) = block_at(rest, 0);
                rest = trim_whitespace(&rest[end..]);
            }
            _ => break,
        }
    }
    Some((target, media_applies(rest)))
}

/// A style rule's block: its declarations, and any nested rules, whose
/// selectors are read on their own, which finds at least what nesting
/// them under this rule would.
fn parse_style_block(
    selectors: &[Token],
    block: &[Token],
    media: bool,
    sheet: &mut Stylesheet,
    nesting: usize,
) -> Result<(), DocumentError> {
    let mut declarations = Vec::new();
    let mut index = 0;
    while index < block.len() {
        match &block[index] {
            Token::Whitespace | Token::Semicolon => index += 1,
            Token::AtKeyword(name) => {
                index = parse_at_rule(
                    block,
                    index,
                    name,
                    false,
                    media,
                    Some(selectors),
                    sheet,
                    nesting,
                )?;
            }
            _ => {
                let start = index;
                while index < block.len()
                    && !matches!(block[index], Token::Semicolon | Token::OpenCurly)
                {
                    index = skip_component(block, index);
                }
                if index < block.len() && block[index] == Token::OpenCurly {
                    if nesting >= MAX_RULE_NESTING {
                        return Err(DocumentError::ResourceLimit);
                    }
                    let (inner, end) = block_at(block, index);
                    parse_style_block(&block[start..index], inner, media, sheet, nesting + 1)?;
                    index = end;
                } else {
                    declarations.extend(parse_declaration(&block[start..index]));
                    index += 1;
                }
            }
        }
    }
    if media && !declarations.is_empty() {
        let declarations: Rc<[Declaration]> = declarations.into();
        for selector in parse_selector_list(selectors, 0) {
            if !selector.pseudo_element {
                sheet.rules.push(Rc::new(StyleRule {
                    selector,
                    declarations: declarations.clone(),
                }));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The reader's cascade

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Precedence {
    tier: u8,
    specificity: (u32, u32, u32),
    order: u32,
}

const TIER_USER_AGENT: u8 = 0;
const TIER_AUTHOR: u8 = 1;
const TIER_INLINE: u8 = 2;
const TIER_AUTHOR_IMPORTANT: u8 = 3;
const TIER_INLINE_IMPORTANT: u8 = 4;

#[derive(Clone, Copy)]
struct Applied {
    precedence: Precedence,
    certainty: Tri,
    effect: Effect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolved {
    Hidden,
    Shown,
    /// Nothing decides; for `visibility`, the parent's value holds.
    Inherited,
}

/// Resolve one property. A hiding declaration counts unless a declaration
/// that certainly applies and shows the content beats it in the cascade.
fn resolve(applied: &[Applied]) -> Resolved {
    let beaten = |hide: &Applied| {
        applied.iter().any(|other| {
            other.certainty == Tri::Yes
                && matches!(other.effect, Effect::Show | Effect::Inherit)
                && other.precedence > hide.precedence
        })
    };
    if applied
        .iter()
        .any(|hide| hide.effect == Effect::Hide && !beaten(hide))
    {
        return Resolved::Hidden;
    }
    let Some(best) = applied
        .iter()
        .filter(|entry| entry.certainty == Tri::Yes && entry.effect != Effect::Neutral)
        .max_by_key(|entry| entry.precedence)
    else {
        return Resolved::Inherited;
    };
    let contested = applied.iter().any(|entry| {
        entry.certainty == Tri::Maybe
            && !matches!(entry.effect, Effect::Neutral | Effect::Show)
            && entry.precedence > best.precedence
    });
    if best.effect == Effect::Show && !contested {
        Resolved::Shown
    } else {
        Resolved::Inherited
    }
}

/// What the reader's cascade gives one element.
pub(super) struct ReaderStyle {
    display: Resolved,
    visibility: Resolved,
    content_visibility: Resolved,
}

enum RuleKey {
    Id(String),
    Class(String),
    Tag(String),
}

/// The rules a chapter applies in cascade order, indexed by the id, class,
/// or element name their rightmost compound requires.
#[derive(Default)]
pub(super) struct Cascade {
    rules: Vec<(Rc<StyleRule>, u32)>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
}

impl Cascade {
    pub(super) fn push_sheet(&mut self, sheet: &Stylesheet) {
        for rule in &sheet.rules {
            let index = self.rules.len();
            let key = rule
                .selector
                .compounds
                .last()
                .and_then(|compound| {
                    compound.parts.iter().find_map(|part| match part {
                        Simple::Id(id) => Some(RuleKey::Id(id.to_lowercase())),
                        _ => None,
                    })
                })
                .or_else(|| {
                    rule.selector.compounds.last().and_then(|compound| {
                        compound.parts.iter().find_map(|part| match part {
                            Simple::Class(class) => Some(RuleKey::Class(class.to_lowercase())),
                            _ => None,
                        })
                    })
                })
                .or_else(|| {
                    rule.selector.compounds.last().and_then(|compound| {
                        compound.parts.iter().find_map(|part| match part {
                            Simple::Type {
                                name: Some(name), ..
                            } => Some(RuleKey::Tag(name.clone())),
                            _ => None,
                        })
                    })
                });
            match key {
                Some(RuleKey::Id(id)) => self.by_id.entry(id).or_default().push(index),
                Some(RuleKey::Class(class)) => self.by_class.entry(class).or_default().push(index),
                Some(RuleKey::Tag(tag)) => self.by_tag.entry(tag).or_default().push(index),
                None => self.universal.push(index),
            }
            self.rules.push((rule.clone(), index as u32 + 1));
        }
    }

    pub(super) fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// The cascade for the element at the top of `stack`: author rules,
    /// its inline style, SVG presentation attributes, and the user-agent
    /// rules that hide content.
    fn evaluate(&self, stack: &[Element], work: &mut u64) -> Result<ReaderStyle, DocumentError> {
        let element = stack.last().expect("an element to style");
        let mut candidates: Vec<usize> = Vec::new();
        for (_, id) in element.values("id") {
            candidates.extend(self.by_id.get(&id.to_lowercase()).into_iter().flatten());
        }
        for (_, classes) in element.values("class") {
            for class in classes.split_ascii_whitespace() {
                candidates.extend(
                    self.by_class
                        .get(&class.to_lowercase())
                        .into_iter()
                        .flatten(),
                );
            }
        }
        candidates.extend(self.by_tag.get(&element.lower).into_iter().flatten());
        candidates.extend(self.universal.iter());
        candidates.sort_unstable();
        candidates.dedup();

        let mut applied: [Vec<Applied>; 3] = Default::default();
        let mut add = |declaration: &Declaration, precedence: Precedence, certainty: Tri| {
            let slot = match declaration.property {
                Property::Display => 0,
                Property::Visibility => 1,
                Property::ContentVisibility => 2,
            };
            applied[slot].push(Applied {
                precedence,
                certainty,
                effect: declaration.effect,
            });
        };
        for index in candidates {
            let (rule, order) = &self.rules[index];
            let certainty = match_complex(&rule.selector, stack, work);
            if certainty == Tri::No {
                continue;
            }
            for declaration in rule.declarations.iter() {
                let tier = if declaration.important {
                    TIER_AUTHOR_IMPORTANT
                } else {
                    TIER_AUTHOR
                };
                add(
                    declaration,
                    Precedence {
                        tier,
                        specificity: rule.selector.specificity,
                        order: *order,
                    },
                    certainty,
                );
            }
        }
        if *work > MAX_MATCH_WORK {
            return Err(DocumentError::ResourceLimit);
        }
        for (prefixed, style) in element.values("style") {
            let certainty = if prefixed { Tri::Maybe } else { Tri::Yes };
            for declaration in inline_declarations(style)? {
                let tier = if declaration.important {
                    TIER_INLINE_IMPORTANT
                } else {
                    TIER_INLINE
                };
                add(
                    &declaration,
                    Precedence {
                        tier,
                        specificity: (0, 0, 0),
                        order: 0,
                    },
                    certainty,
                );
            }
        }
        // SVG presentation attributes: author styles that every rule beats.
        let presentation = Precedence {
            tier: TIER_AUTHOR,
            specificity: (0, 0, 0),
            order: 0,
        };
        for (prefixed, value) in element.values("display") {
            let value = value.trim().to_ascii_lowercase();
            let effect = if value == "none" {
                Effect::Hide
            } else if DISPLAY_KEYWORDS.contains(&value.as_str()) {
                Effect::Show
            } else {
                Effect::Neutral
            };
            let certainty = if prefixed { Tri::Maybe } else { Tri::Yes };
            add(
                &Declaration {
                    property: Property::Display,
                    effect,
                    important: false,
                },
                presentation,
                certainty,
            );
        }
        for (prefixed, value) in element.values("visibility") {
            let effect = match value.trim().to_ascii_lowercase().as_str() {
                "hidden" | "collapse" => Effect::Hide,
                "visible" => Effect::Show,
                "inherit" => Effect::Inherit,
                _ => Effect::Neutral,
            };
            let certainty = if prefixed { Tri::Maybe } else { Tri::Yes };
            add(
                &Declaration {
                    property: Property::Visibility,
                    effect,
                    important: false,
                },
                presentation,
                certainty,
            );
        }
        // User-agent rules (HTML's rendering section) that hide content.
        let user_agent = Precedence {
            tier: TIER_USER_AGENT,
            specificity: (0, 0, 0),
            order: 0,
        };
        let has = |name: &str| element.values(name).next().is_some();
        let hidden_by_user_agent = has("hidden")
            || (element.lower == "dialog" && !has("open"))
            || (element.lower == "audio" && !has("controls"))
            || matches!(
                element.lower.as_str(),
                "area"
                    | "base"
                    | "basefont"
                    | "datalist"
                    | "head"
                    | "link"
                    | "meta"
                    | "noembed"
                    | "noframes"
                    | "param"
                    | "rp"
                    | "script"
                    | "style"
                    | "template"
                    | "title"
            );
        if hidden_by_user_agent {
            add(
                &Declaration {
                    property: Property::Display,
                    effect: Effect::Hide,
                    important: false,
                },
                user_agent,
                Tri::Yes,
            );
        }
        Ok(ReaderStyle {
            display: resolve(&applied[0]),
            visibility: resolve(&applied[1]),
            content_visibility: resolve(&applied[2]),
        })
    }
}

// ---------------------------------------------------------------------------
// AnyDoc's cascade

const ANYDOC_INLINE_PRIORITY: u32 = 100_000;
const ANYDOC_IMPORTANT_PRIORITY: u32 = 1_000_000;

/// AnyDoc 0.2.4's stylesheet (`shared::html::Stylesheet`), reduced to
/// `display` and ported with its parsing: comments stripped, rules split at
/// every `}`, and only selectors without a space, `:`, or `[` kept, read as
/// `tag`, `.class`, or `tag.class`.
#[derive(Default)]
pub(super) struct AnyDocCascade {
    by_tag: HashMap<String, AnyDocRules>,
    by_class: HashMap<String, AnyDocRules>,
    by_both: HashMap<(String, String), AnyDocRules>,
    order: u32,
}

/// AnyDoc rules as (priority, order, hides), keyed by lowercased tag,
/// class, or both.
type AnyDocRules = Vec<(u32, u32, bool)>;

/// AnyDoc's `strip_css_comments`, whose closing search starts at the
/// opener, so `/*/` closes itself.
fn anydoc_strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// AnyDoc's `parse_declarations`, for `display` alone: the last value in
/// each tier (normal and `!important`), `true` for `none`.
fn anydoc_display(body: &str) -> (Option<bool>, Option<bool>) {
    let (mut normal, mut important) = (None, None);
    for declaration in body.split(';') {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let mut value = value.trim().to_ascii_lowercase();
        let mut is_important = false;
        if let Some(bang) = value.find('!') {
            if value[bang + 1..].trim() == "important" {
                is_important = true;
            }
            value.truncate(bang);
            value.truncate(value.trim_end().len());
        }
        if name == "display" {
            let slot = if is_important {
                &mut important
            } else {
                &mut normal
            };
            *slot = Some(value == "none");
        }
    }
    (normal, important)
}

impl AnyDocCascade {
    /// Add one stylesheet's text, as `Stylesheet::add` does.
    pub(super) fn add(&mut self, css: &str) {
        let css = anydoc_strip_comments(css);
        for chunk in css.split('}') {
            let Some((selectors, body)) = chunk.split_once('{') else {
                continue;
            };
            let (normal, important) = anydoc_display(body);
            if normal.is_none() && important.is_none() {
                continue;
            }
            for selector in selectors.split(',') {
                let selector = selector.trim();
                if selector.is_empty()
                    || selector.contains(' ')
                    || selector.contains(':')
                    || selector.contains('[')
                {
                    continue;
                }
                let (tag, class) = match selector.split_once('.') {
                    Some((tag, class)) => (
                        (!tag.is_empty()).then(|| tag.to_ascii_lowercase()),
                        Some(class.to_string()),
                    ),
                    None => (Some(selector.to_ascii_lowercase()), None),
                };
                let specificity = u32::from(class.is_some()) * 10 + u32::from(tag.is_some());
                for (hides, base) in [(normal, 0), (important, ANYDOC_IMPORTANT_PRIORITY)] {
                    let Some(hides) = hides else {
                        continue;
                    };
                    self.order += 1;
                    let entry = (base + specificity, self.order, hides);
                    match (&tag, &class) {
                        (Some(tag), Some(class)) => self
                            .by_both
                            .entry((tag.clone(), class.clone()))
                            .or_default()
                            .push(entry),
                        (Some(tag), None) => {
                            self.by_tag.entry(tag.clone()).or_default().push(entry)
                        }
                        (None, Some(class)) => {
                            self.by_class.entry(class.clone()).or_default().push(entry)
                        }
                        (None, None) => {}
                    }
                }
            }
        }
    }

    /// Whether AnyDoc omits an element it styles (`element_props`): its
    /// `display: none` wins, by priority and then source order.
    fn hides(&self, element: &Element) -> bool {
        let classes: Vec<&str> = element
            .first("class")
            .map(|classes| classes.split_whitespace().collect())
            .unwrap_or_default();
        let mut best: Option<(u32, u32, bool)> = None;
        let mut consider = |entries: Option<&AnyDocRules>| {
            for &entry in entries.into_iter().flatten() {
                if best.is_none_or(|current| (entry.0, entry.1) > (current.0, current.1)) {
                    best = Some(entry);
                }
            }
        };
        consider(self.by_tag.get(&element.local));
        for class in &classes {
            consider(self.by_class.get(*class));
            consider(
                self.by_both
                    .get(&(element.local.clone(), (*class).to_string())),
            );
        }
        if let Some(style) = element.first("style") {
            let (normal, important) = anydoc_display(style);
            for (hides, priority, order) in [
                (normal, ANYDOC_INLINE_PRIORITY, u32::MAX - 1),
                (
                    important,
                    ANYDOC_IMPORTANT_PRIORITY + ANYDOC_INLINE_PRIORITY,
                    u32::MAX,
                ),
            ] {
                if let Some(hides) = hides {
                    if best.is_none_or(|current| (priority, order) > (current.0, current.1)) {
                        best = Some((priority, order, hides));
                    }
                }
            }
        }
        best.is_some_and(|(_, _, hides)| hides)
    }
}

// ---------------------------------------------------------------------------
// The chapter walk

/// How AnyDoc's HTML walker (`shared::html`) treats an element's children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// The chosen `html` element: only its first `body` is converted.
    Root,
    /// Each child is styled and walked (`walk_elem`); text converts.
    Walk,
    /// `ul` or `ol`: only `li` children, walked without styling the item.
    List,
    /// `table`: row groups, rows, and the first caption, unstyled.
    Table,
    RowGroup,
    Row,
    /// `pre` or `math`: all text below converts, whatever its style.
    Whole,
    /// Nothing below converts, but a reader may show it: an element AnyDoc
    /// styles hidden, `noscript`, or content in a position its walker skips,
    /// such as text or a paragraph directly in a list or table.
    Omitted,
    /// Nothing below converts, and a reader shows none of it either.
    Dropped,
}

/// Text a reader shows only in some cases: SVG descriptions, like image alt
/// text, and ruby parentheses when they are only punctuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exempt {
    None,
    Description,
    RubyParenthesis,
}

struct Open {
    children: usize,
    reach: Reach,
    caption_seen: bool,
    /// The element is not rendered: `display: none` on it or an ancestor,
    /// or fallback content of a media element.
    undisplayed: bool,
    invisible: bool,
    /// `content-visibility: hidden`: nothing inside is rendered.
    contents_hidden: bool,
    /// Children are fallback content a reader replaces.
    fallback: bool,
    in_svg: bool,
    exempt: Exempt,
    /// How AnyDoc's inline run treats the element (see [`Run`]).
    opens_run: bool,
    flush_after: bool,
    boundary_after: bool,
}

/// One inline run of AnyDoc's walker (a `Builder`): the text it last
/// added, and whether a reader starts a new block since then without AnyDoc
/// starting a new paragraph. Text added across such a boundary with no white
/// space between runs together in the Markdown, as "Balance due1,250.00".
#[derive(Default)]
struct Run {
    last: Option<char>,
    boundary: bool,
}

impl Run {
    fn flush(&mut self) {
        *self = Run::default();
    }

    /// Add text to the run; whether it runs into the text before it.
    fn add(&mut self, text: &str) -> bool {
        let kept = || text.chars().filter(|character| anydoc_keeps(*character));
        let (Some(first), Some(last)) = (kept().next(), kept().next_back()) else {
            return false;
        };
        if kept().all(char::is_whitespace) {
            if self.last.is_some() {
                self.last = Some(' ');
            }
            return false;
        }
        let fused = self.boundary
            && self.last.is_some_and(|previous| !previous.is_whitespace())
            && !first.is_whitespace();
        self.last = Some(last);
        self.boundary = false;
        fused
    }
}

/// Whether AnyDoc keeps a character of text: `clean_text` drops soft hyphens,
/// zero-width spaces, byte order marks, and control characters other than
/// tabs and line ends.
fn anydoc_keeps(character: char) -> bool {
    !matches!(character, '\u{ad}' | '\u{200b}' | '\u{feff}')
        && (!character.is_control() || matches!(character, '\t' | '\n' | '\r'))
}

/// Whether an image source is an absolute URI (`is_absolute_uri`): a scheme,
/// and not a Windows drive path.
fn anydoc_absolute_uri(source: &str) -> bool {
    let Some((scheme, _)) = source.split_once(':') else {
        return false;
    };
    let mut characters = scheme.chars();
    let scheme = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        });
    let bytes = source.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    scheme && !drive
}

/// AnyDoc's container elements (`is_container_tag`): walked through, with a
/// new paragraph only when they hold a block element.
fn anydoc_container(local: &str) -> bool {
    matches!(
        local,
        "div"
            | "section"
            | "article"
            | "aside"
            | "main"
            | "nav"
            | "header"
            | "footer"
            | "figure"
            | "figcaption"
            | "center"
            | "details"
            | "summary"
            | "li"
            | "dl"
            | "dt"
            | "dd"
            | "body"
    )
}

/// AnyDoc's block elements (`is_block_tag`).
fn anydoc_block(local: &str) -> bool {
    anydoc_container(local)
        || matches!(
            local,
            "p" | "ul"
                | "ol"
                | "table"
                | "blockquote"
                | "pre"
                | "hr"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
        )
}

/// For each element of a chapter in document order, whether a child element
/// is one of AnyDoc's blocks (`has_block_children`).
fn block_children(chapter: &[u8]) -> Result<Vec<bool>, DocumentError> {
    let mut xml = quick_xml::Reader::from_reader(std::io::Cursor::new(chapter));
    xml.config_mut().trim_text(false);
    xml.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut flags: Vec<bool> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    loop {
        let event = xml
            .read_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                open.pop();
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return Ok(flags),
            _ => {
                buffer.clear();
                continue;
            }
        };
        let name = element.name();
        let local = String::from_utf8_lossy(super::xml_local_name(name.as_ref()));
        if let Some(&parent) = open.last() {
            flags[parent] |= anydoc_block(&local);
        }
        if start {
            open.push(flags.len());
        }
        flags.push(false);
        buffer.clear();
    }
}

/// Elements whose text no reader renders and AnyDoc never converts: the
/// elements AnyDoc's walker skips that HTML's rendering rules hide, and the
/// elements it reads for their attributes alone.
fn never_rendered(element: &Element) -> bool {
    matches!(
        element.local.as_str(),
        "script" | "style" | "head" | "template" | "hr" | "br" | "img" | "image"
    )
}

/// An element in a position AnyDoc's walker skips.
fn left_out(element: &Element) -> Reach {
    if never_rendered(element) {
        Reach::Dropped
    } else {
        Reach::Omitted
    }
}

fn walked_reach(element: &Element, anydoc: &AnyDocCascade) -> Reach {
    if never_rendered(element) {
        return Reach::Dropped;
    }
    if anydoc.hides(element) {
        return Reach::Omitted;
    }
    match element.local.as_str() {
        // A reader without scripting, as most are, shows it.
        "noscript" => Reach::Omitted,
        "pre" | "math" => Reach::Whole,
        "ul" | "ol" => Reach::List,
        "table" => Reach::Table,
        _ => Reach::Walk,
    }
}

/// How a chapter's text fares between a reading system and AnyDoc.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ChapterText {
    /// Text a reader hides converts.
    pub(super) converts_hidden: bool,
    /// Text a reader shows does not convert.
    pub(super) drops_shown: bool,
    /// Text a reader shows in separate blocks runs together, with no space
    /// between, in AnyDoc's Markdown.
    pub(super) fuses_blocks: bool,
}

/// Compare what a reading system shows of a chapter with what AnyDoc
/// converts. The chapter must already be known to be well formed.
pub(super) fn chapter_text(
    chapter: &[u8],
    reader: &Cascade,
    anydoc: &AnyDocCascade,
    work: &mut u64,
) -> Result<ChapterText, DocumentError> {
    let mut xml = quick_xml::Reader::from_reader(std::io::Cursor::new(chapter));
    xml.config_mut().trim_text(false);
    xml.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut elements: Vec<Element> = Vec::new();
    let mut open: Vec<Open> = Vec::new();
    let mut top_level = 0usize;
    let mut root_taken = false;
    let mut body_taken = false;
    let mut found = ChapterText::default();
    let blocks = block_children(chapter)?;
    let mut element_index = 0usize;
    let mut runs: Vec<Run> = Vec::new();
    loop {
        let event = xml
            .read_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let (start, text) = match event {
            quick_xml::events::Event::Start(start) => (Some((start, true)), None),
            quick_xml::events::Event::Empty(start) => (Some((start, false)), None),
            quick_xml::events::Event::End(_) => {
                elements.pop();
                if let Some(closed) = open.pop() {
                    if closed.opens_run {
                        runs.pop();
                    }
                    if let Some(run) = runs.last_mut() {
                        if closed.flush_after {
                            run.flush();
                        }
                        run.boundary |= closed.boundary_after;
                    }
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Text(text) => (
                None,
                Some(String::from_utf8_lossy(text.as_ref()).into_owned()),
            ),
            quick_xml::events::Event::CData(text) => (
                None,
                Some(String::from_utf8_lossy(text.as_ref()).into_owned()),
            ),
            quick_xml::events::Event::GeneralRef(reference) => (
                None,
                Some(super::anydoc_entity_text(&String::from_utf8_lossy(
                    reference.as_ref(),
                ))),
            ),
            quick_xml::events::Event::Eof => return Ok(found),
            _ => {
                buffer.clear();
                continue;
            }
        };
        if let Some(text) = text {
            if open.last().is_some_and(|state| state.reach == Reach::Walk) {
                if let Some(run) = runs.last_mut() {
                    found.fuses_blocks |= run.add(&text);
                }
            }
            let state = open
                .last()
                .filter(|_| text.chars().any(|character| !character.is_whitespace()));
            if let Some(state) = state {
                let hidden =
                    state.undisplayed || state.invisible || state.contents_hidden || state.fallback;
                match state.reach {
                    Reach::Walk | Reach::Whole if hidden => {
                        let shown_anyway = match state.exempt {
                            Exempt::None => false,
                            Exempt::Description => true,
                            Exempt::RubyParenthesis => !text.chars().any(char::is_alphanumeric),
                        };
                        found.converts_hidden |= !shown_anyway;
                    }
                    Reach::Walk | Reach::Whole | Reach::Dropped => {}
                    // Text in a list, a table, a row group, a row, or the
                    // root element outside the body is skipped, like
                    // everything in an omitted element.
                    Reach::Root
                    | Reach::List
                    | Reach::Table
                    | Reach::RowGroup
                    | Reach::Row
                    | Reach::Omitted => {
                        found.drops_shown |= !hidden && state.exempt == Exempt::None;
                    }
                }
                if found.converts_hidden && found.drops_shown && found.fuses_blocks {
                    return Ok(found);
                }
            }
            buffer.clear();
            continue;
        }
        let Some((start, has_children)) = start else {
            buffer.clear();
            continue;
        };
        if elements.len() >= MAX_CHAPTER_DEPTH {
            return Err(DocumentError::ResourceLimit);
        }
        let has_blocks = blocks.get(element_index).copied().unwrap_or(false);
        element_index += 1;
        let local =
            String::from_utf8_lossy(super::xml_local_name(start.name().as_ref())).into_owned();
        let attributes = super::xml_attributes(&start)
            .into_iter()
            .map(|attribute| {
                (
                    String::from_utf8_lossy(attribute.local()).to_ascii_lowercase(),
                    attribute.prefixed(),
                    attribute.value,
                )
            })
            .collect();
        let index = match open.last_mut() {
            Some(parent) => {
                parent.children += 1;
                parent.children
            }
            None => {
                top_level += 1;
                top_level
            }
        };
        let element = Element::new(&local, attributes, index);
        let reach = match open.last_mut() {
            None => {
                if element.local == "html" && !root_taken {
                    root_taken = true;
                    Reach::Root
                } else {
                    left_out(&element)
                }
            }
            Some(parent) => match parent.reach {
                Reach::Root => {
                    if element.local == "body" && !body_taken {
                        body_taken = true;
                        Reach::Walk
                    } else {
                        left_out(&element)
                    }
                }
                Reach::Walk => walked_reach(&element, anydoc),
                Reach::List => {
                    if element.local == "li" {
                        Reach::Walk
                    } else {
                        left_out(&element)
                    }
                }
                Reach::Table => match element.local.as_str() {
                    "thead" | "tbody" | "tfoot" => Reach::RowGroup,
                    "tr" => Reach::Row,
                    "caption" if !parent.caption_seen => {
                        parent.caption_seen = true;
                        Reach::Walk
                    }
                    _ => left_out(&element),
                },
                Reach::RowGroup => {
                    if element.local == "tr" {
                        Reach::Row
                    } else {
                        left_out(&element)
                    }
                }
                Reach::Row => {
                    if matches!(element.local.as_str(), "td" | "th") {
                        Reach::Walk
                    } else {
                        left_out(&element)
                    }
                }
                Reach::Whole => Reach::Whole,
                Reach::Omitted => left_out(&element),
                Reach::Dropped => Reach::Dropped,
            },
        };
        // How the element meets AnyDoc's inline run. Paragraphs, headings,
        // quotes, list items, cells, captions, and the body get runs of
        // their own; lists, tables, `pre`, rules, and containers holding
        // blocks end the current paragraph; any other container is walked
        // inline although a reader starts a new block.
        let parent_reach = open.last().map(|parent| parent.reach);
        let local = element.local.as_str();
        let (mut opens_run, mut flush_after, mut boundary_after) = (false, false, false);
        match (parent_reach, reach) {
            (Some(Reach::Root), Reach::Walk)
            | (Some(Reach::List | Reach::Table | Reach::Row), Reach::Walk) => opens_run = true,
            (Some(Reach::Walk), _) => {
                let run = runs.last_mut();
                match (local, reach) {
                    ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote", Reach::Walk) => {
                        if let Some(run) = run {
                            run.flush();
                        }
                        opens_run = true;
                        flush_after = true;
                    }
                    (_, Reach::List | Reach::Table | Reach::Whole) | ("hr", _) => {
                        if let Some(run) = run {
                            run.flush();
                        }
                        flush_after = true;
                    }
                    ("br", Reach::Dropped) => {
                        if let Some(run) = run.filter(|run| run.last.is_some()) {
                            run.last = Some('\n');
                        }
                    }
                    // AnyDoc writes a packaged image as its alt text, inline,
                    // and an image from outside the package as Markdown image
                    // markup, which keeps the text on either side apart.
                    ("img" | "image", Reach::Dropped) if !anydoc.hides(&element) => {
                        let source = element
                            .first("src")
                            .or_else(|| element.first("href"))
                            .unwrap_or("");
                        if let Some(run) = run {
                            if anydoc_absolute_uri(source) {
                                if run.last.is_some() {
                                    run.last = Some(' ');
                                }
                            } else {
                                let alt = element.first("alt").unwrap_or("").trim();
                                found.fuses_blocks |= run.add(alt);
                            }
                        }
                    }
                    (container, Reach::Walk) if anydoc_container(container) => {
                        if let Some(run) = run {
                            if has_blocks {
                                run.flush();
                            } else {
                                run.boundary = true;
                            }
                        }
                        if has_blocks {
                            flush_after = true;
                        } else {
                            boundary_after = true;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        // An empty element holds no text to check.
        if !has_children {
            if let Some(run) = runs.last_mut() {
                if flush_after {
                    run.flush();
                }
                run.boundary |= boundary_after;
            }
            buffer.clear();
            continue;
        }
        if opens_run {
            runs.push(Run::default());
        }
        elements.push(element);
        let parent = open.last();
        let in_svg = parent.is_some_and(|parent| parent.in_svg)
            || elements
                .last()
                .is_some_and(|element| element.lower == "svg");
        let state = if reach == Reach::Dropped {
            // Nothing below converts or shows, so its style does not matter.
            Open {
                children: 0,
                reach,
                caption_seen: false,
                undisplayed: false,
                invisible: false,
                contents_hidden: false,
                fallback: false,
                in_svg,
                exempt: Exempt::None,
                opens_run,
                flush_after,
                boundary_after,
            }
        } else {
            let style = reader.evaluate(&elements, work)?;
            let element = elements.last().expect("the element just pushed");
            let inherited_invisible = parent.is_some_and(|parent| parent.invisible);
            let exempt = match parent.map(|parent| parent.exempt) {
                Some(exempt) if exempt != Exempt::None => exempt,
                _ if in_svg && matches!(element.lower.as_str(), "title" | "desc" | "metadata") => {
                    Exempt::Description
                }
                _ if element.lower == "rp" => Exempt::RubyParenthesis,
                _ => Exempt::None,
            };
            Open {
                children: 0,
                reach,
                caption_seen: false,
                undisplayed: parent.is_some_and(|parent| {
                    parent.undisplayed || parent.contents_hidden || parent.fallback
                }) || style.display == Resolved::Hidden,
                invisible: match style.visibility {
                    Resolved::Hidden => true,
                    Resolved::Shown => false,
                    Resolved::Inherited => inherited_invisible,
                },
                contents_hidden: style.content_visibility == Resolved::Hidden,
                fallback: matches!(element.lower.as_str(), "audio" | "video" | "canvas"),
                in_svg,
                exempt,
                opens_run,
                flush_after,
                boundary_after,
            }
        };
        open.push(state);
        buffer.clear();
    }
}

#[cfg(test)]
pub(super) fn cascade_for(css: &[&str]) -> (Cascade, AnyDocCascade) {
    let mut reader = Cascade::default();
    let mut anydoc = AnyDocCascade::default();
    for sheet in css {
        reader.push_sheet(&parse_stylesheet(sheet).expect("stylesheet"));
        anydoc.add(sheet);
    }
    (reader, anydoc)
}

/// Whether a URL in a stylesheet loads something from outside the package:
/// a network location, a local file, or a script. A `data:` URL is
/// self-contained.
fn loads_from_outside(target: &str) -> bool {
    let target = target.trim().to_ascii_lowercase();
    target.starts_with("//")
        || ["http:", "https:", "ftp:", "file:", "javascript:"]
            .iter()
            .any(|scheme| target.starts_with(scheme))
}

/// Whether a stylesheet or `style` attribute loads anything from outside the
/// package, through `url()`, `image-set()`, or `@import`. Comments, string
/// values such as `content`, and `@namespace` identifiers load nothing. The
/// text is scanned as a token stream, without holding the tokens.
pub(super) fn references_external(css: &str) -> bool {
    let mut tokenizer = Tokenizer::new(css);
    // Open functions that take URLs, by nesting depth, and whether an
    // `@import` awaits its target.
    let mut depth = 0usize;
    let mut url_function_depths: Vec<usize> = Vec::new();
    let mut import_pending = false;
    while let Some(token) = tokenizer.next_token() {
        match &token {
            Token::Url(target) if loads_from_outside(target) => return true,
            Token::Str(target)
                if (import_pending || !url_function_depths.is_empty())
                    && loads_from_outside(target) =>
            {
                return true;
            }
            _ => {}
        }
        match token {
            Token::Whitespace => continue,
            Token::AtKeyword(name) => {
                import_pending = name.eq_ignore_ascii_case("import");
                continue;
            }
            Token::Function(function) => {
                depth += 1;
                if ["url", "src", "image", "image-set", "-webkit-image-set"]
                    .iter()
                    .any(|name| function.eq_ignore_ascii_case(name))
                {
                    url_function_depths.push(depth);
                }
            }
            Token::OpenParen | Token::OpenSquare | Token::OpenCurly => depth += 1,
            Token::CloseParen | Token::CloseSquare | Token::CloseCurly => {
                if url_function_depths.last() == Some(&depth) {
                    url_function_depths.pop();
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        import_pending = false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(body: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Head text</title></head><body>{body}</body></html>"#
        )
        .into_bytes()
    }

    fn walk(sheets: &[&str], body: &str) -> ChapterText {
        let (reader, anydoc) = cascade_for(sheets);
        let mut work = 0;
        chapter_text(&chapter(body), &reader, &anydoc, &mut work).expect("chapter walk")
    }

    /// Whether text a reader hides converts, with these stylesheets.
    fn converts_hidden(sheets: &[&str], body: &str) -> bool {
        walk(sheets, body).converts_hidden
    }

    /// Whether text a reader shows does not convert.
    fn drops_shown(sheets: &[&str], body: &str) -> bool {
        walk(sheets, body).drops_shown
    }

    #[test]
    fn tokenizer_reads_escapes_comments_strings_and_urls() {
        assert_eq!(
            tokenize(r"hidd\65n").unwrap(),
            vec![Token::Ident("hidden".into())]
        );
        assert_eq!(
            tokenize(r"dis\play").unwrap(),
            vec![Token::Ident("display".into())]
        );
        assert_eq!(
            tokenize(r".\73 ecret").unwrap(),
            vec![Token::Delim('.'), Token::Ident("secret".into())]
        );
        assert_eq!(
            tokenize("a/* c */b").unwrap(),
            vec![Token::Ident("a".into()), Token::Ident("b".into())]
        );
        assert_eq!(
            tokenize(r#""a::b }""#).unwrap(),
            vec![Token::Str("a::b }".into())]
        );
        assert_eq!(
            tokenize("Url( v.css )").unwrap(),
            vec![Token::Url("v.css".into())]
        );
        assert_eq!(
            tokenize("url( \"v.css\" )").unwrap()[0],
            Token::Function("url".into())
        );
    }

    #[test]
    fn inline_styles_hide_as_readers_parse_them() {
        for style in [
            "visibility:/**/hidden",
            r"visibility:hidd\65n",
            r"dis\play:none",
            "display:none/**/",
            "VISIBILITY : Hidden !important",
            "content-visibility: hidden",
            "display: var(--hide)",
        ] {
            assert!(
                converts_hidden(&[], &format!(r#"<p style="{style}">SECRET</p>"#)),
                "{style}"
            );
        }
        // AnyDoc omits what a plain `display: none` hides, as readers do.
        for style in [
            "display: none",
            "display:NONE",
            "visibility: visible",
            "display: block",
        ] {
            assert!(
                !converts_hidden(&[], &format!(r#"<p style="{style}">SECRET</p>"#)),
                "{style}"
            );
        }
    }

    #[test]
    fn selectors_match_what_readers_match() {
        let body = r#"<p class="secret" title="a::b }" data-x="1">SECRET</p><p>VISIBLE</p>"#;
        for rule in [
            r"p.\73 ecret { visibility: hidden }",
            r".\secret { visibility: hidden }",
            "*|p.secret { visibility: hidden }",
            "*|* { visibility: hidden }",
            r#"p[title="a::b }"] { visibility: hidden }"#,
            "p[data-x] { display: none }",
            "body > p.secret { display: none }",
            "html p:first-child { display: none }",
            "p:nth-child(2n+1) { display: none }",
            "p:not(.secret) { visibility: hidden }",
            "p:is(.secret, .other) { visibility: hidden }",
            "@media screen { .secret { visibility: hidden } }",
            "@media not print { .secret { visibility: hidden } }",
            "@supports (display: grid) { .secret { visibility: hidden } }",
            ".secret { @media screen { visibility: hidden } }",
            "h1 + p { visibility: hidden }",
        ] {
            assert!(converts_hidden(&[rule], body), "{rule}");
        }
        for rule in [
            // AnyDoc applies these too, so the text is omitted, not kept.
            ".secret { display: none }",
            "p.secret { display: none }",
            // Rules that hide nothing here.
            "p::before { display: none }",
            "p:first-letter { visibility: hidden }",
            ".secret:hover { visibility: hidden }",
            "div > p { visibility: hidden }",
            "p:not(p) { visibility: hidden }",
            "[hidden] { display: none !important }",
            "@media print { .secret { visibility: hidden } }",
            "@media amzn-mobi { .secret { visibility: hidden } }",
            "@font-face { font-family: x; visibility: hidden }",
            "@keyframes fade { to { visibility: hidden } }",
            ".secret { visibility: frobnicated }",
        ] {
            assert!(!converts_hidden(&[rule], body), "{rule}");
        }
    }

    #[test]
    fn attribute_selectors_hide_only_what_they_match() {
        let pagebreak = |text: &str| {
            format!(r#"<p>Before <span epub:type="pagebreak" title="12">{text}</span> after</p>"#)
        };
        let rule = r#"@namespace epub "http://www.idpf.org/2007/ops"; span[epub|type~="pagebreak"] { display: none }"#;
        assert!(converts_hidden(&[rule], &pagebreak("12")));
        assert!(!converts_hidden(&[rule], &pagebreak("")));
        assert!(!converts_hidden(
            &["span[data-kind=\"pagenum\"] { display: none }"],
            &pagebreak("12")
        ));
    }

    #[test]
    fn the_cascade_decides_between_hiding_and_showing() {
        let body = r#"<div class="box">BOX<p class="secret">SECRET</p></div>"#;
        // A more specific rule shows the paragraph; the box keeps its text.
        assert!(!converts_hidden(
            &[".secret { visibility: hidden } p.secret { visibility: visible }"],
            r#"<p class="secret">SECRET</p>"#
        ));
        assert!(converts_hidden(
            &["p.secret { visibility: visible } .secret { visibility: hidden !important }"],
            r#"<p class="secret">SECRET</p>"#
        ));
        // A showing rule that may not apply does not override.
        assert!(converts_hidden(
            &[".secret { visibility: hidden } h1 + p { visibility: visible }"],
            r#"<p class="secret">SECRET</p>"#
        ));
        // Visibility is inherited unless a child restores it.
        assert!(!converts_hidden(
            &[".box { visibility: hidden } .secret { visibility: visible }"],
            r#"<div class="box"><p class="secret">SECRET</p></div>"#
        ));
        assert!(converts_hidden(
            &[".box { visibility: hidden } .secret { visibility: visible }"],
            body
        ));
        // Later sheets win ties.
        assert!(!converts_hidden(
            &[
                ".secret { visibility: hidden }",
                ".secret { visibility: visible }"
            ],
            r#"<p class="secret">SECRET</p>"#
        ));
    }

    #[test]
    fn user_agent_rules_hide_content() {
        assert!(converts_hidden(&[], r#"<p hidden="">SECRET</p>"#));
        assert!(converts_hidden(
            &[],
            r#"<p hidden="until-found">SECRET</p>"#
        ));
        assert!(!converts_hidden(
            &["p { display: block }"],
            r#"<p hidden="">SHOWN BY THE AUTHOR</p>"#
        ));
        assert!(converts_hidden(&[], "<dialog>SECRET</dialog>"));
        assert!(!converts_hidden(&[], "<dialog open=\"\">SHOWN</dialog>"));
        assert!(converts_hidden(&[], "<video>FALLBACK TEXT</video>"));
        assert!(converts_hidden(&[], "<p><ruby>X<rp>SECRET</rp></ruby></p>"));
        assert!(!converts_hidden(
            &[],
            "<p><ruby>X<rp>(</rp><rt>ex</rt><rp>)</rp></ruby></p>"
        ));
        // SVG presentation attributes, and descriptions treated like alt text.
        assert!(converts_hidden(
            &[],
            r#"<svg><text display="none">SECRET</text></svg>"#
        ));
        assert!(converts_hidden(
            &[],
            r#"<svg><text visibility="hidden">SECRET</text></svg>"#
        ));
        assert!(!converts_hidden(
            &[],
            "<svg><title>Figure</title><desc>A chart</desc></svg>"
        ));
        // Text outside the body never converts.
        assert!(!converts_hidden(
            &["title { visibility: hidden }"],
            "<p>BODY</p>"
        ));
    }

    #[test]
    fn anydoc_styles_only_the_elements_it_walks() {
        // AnyDoc drops what it styles hidden, as a reader hides it.
        assert!(!converts_hidden(
            &[],
            r#"<div style="display:none">GONE</div>"#
        ));
        // It reads list items, table parts, and the body without styling
        // them, and takes `pre` and `math` text whole.
        assert!(converts_hidden(
            &[],
            r#"<ul><li style="display:none">SECRET</li></ul>"#
        ));
        assert!(converts_hidden(
            &[],
            r#"<table><tr><td style="display:none">SECRET</td></tr></table>"#
        ));
        assert!(converts_hidden(
            &["body { display: none }"],
            "<p>SECRET</p>"
        ));
        assert!(converts_hidden(
            &[],
            r#"<pre>code <span style="display:none">SECRET</span></pre>"#
        ));
        // Content AnyDoc never reaches converts nothing.
        assert!(!converts_hidden(
            &[".x { visibility: hidden }"],
            r#"<ul><p class="x">DROPPED</p></ul>"#
        ));
        // Script and style text inside `pre` is hidden but converts.
        assert!(converts_hidden(&[], "<pre>a<script>SECRET</script></pre>"));
    }

    #[test]
    fn blocks_anydoc_runs_together_are_found() {
        let fuses = |body: &str| walk(&[], body).fuses_blocks;
        // A container without block children is walked inline, so adjacent
        // blocks with no white space between them run together.
        for body in [
            "<div>Balance due</div><div>1,250.00</div>",
            "<dl><dt>Tax year</dt><dd>2023</dd></dl>",
            "<p><span>Paid</span><div>300.00</div></p>",
            "Intro<div>More</div>",
            "<section>One</section>Two",
            "A<div/>B",
            "<ul><li><div>Item</div><div>Note</div></li></ul>",
            // A packaged image converts as its alt text.
            r#"<figure><img src="chart.png" alt="chart"/><figcaption>Figure 2</figcaption></figure>"#,
            r#"<div><img src="total.png" alt="Total"/></div><div>1,250.00</div>"#,
        ] {
            assert!(fuses(body), "{body}");
        }
        // White space, paragraphs, blocks inside the containers, list items,
        // cells, and inline elements keep text apart as a reader does.
        for body in [
            "<div>Balance due</div>\n<div>1,250.00</div>",
            "<p>A</p><p>B</p>",
            "<div><p>A</p></div><div><p>B</p></div>",
            "<span>A</span><span>B</span>",
            "<ul><li>A</li><li>B</li></ul>",
            "<table><tr><td>A</td><td>B</td></tr></table>",
            "<div>A<br/>B</div>",
            "<div>A</div><p>B</p>",
            "<div>A </div><div>B</div>",
            "<h1>Title</h1><div>Body</div>",
            // An image from outside the package is Markdown image markup; an
            // image without alt text converts as nothing; AnyDoc drops
            // zero-width spaces.
            r#"<figure><img src="https://example.com/chart.png" alt="chart"/><figcaption>Figure 2</figcaption></figure>"#,
            r#"<figure><img src="chart.png" alt=" "/><figcaption>Figure 2</figcaption></figure>"#,
            r#"<figure><img src="chart.png"/><figcaption>Figure 2</figcaption></figure>"#,
            "<div>A</div><div>\u{200b}</div>",
        ] {
            assert!(!fuses(body), "{body}");
        }
    }

    #[test]
    fn text_readers_show_and_anydoc_skips_is_found() {
        // AnyDoc reads only `li` children of a list, and only row groups,
        // rows, cells, and the first caption of a table.
        for body in [
            "<ul>LOOSE TEXT<li>item</li></ul>",
            "<ol><li>item</li><p>A PARAGRAPH</p></ol>",
            "<table>LOOSE<tr><td>cell</td></tr></table>",
            "<table><tr><td>cell</td></tr><div>A BLOCK</div></table>",
            "<table><tbody><p>X</p><tr><td>cell</td></tr></tbody></table>",
            "<table><tr><td>cell</td><div>X</div></tr></table>",
            "<table><caption>First</caption><caption>SECOND</caption></table>",
            "<noscript>SHOWN WITHOUT SCRIPTS</noscript>",
        ] {
            assert!(drops_shown(&[], body), "{body}");
            assert!(!converts_hidden(&[], body), "{body}");
        }
        // Rules AnyDoc applies and readers do not: its split at every `}`
        // applies the second rule of a media block everywhere.
        let kindle = "@media amzn-mobi { .kf8 { font-size: 1em } .mobi-hidden { display: none } }";
        assert!(drops_shown(
            &[kindle],
            r#"<p class="mobi-hidden">EPUB TEXT</p>"#
        ));
        assert!(!drops_shown(&[kindle], r#"<p class="kf8">EPUB TEXT</p>"#));
        // Skipped text a reader hides too, whitespace, never-rendered
        // elements, and descriptions are not lost.
        for body in [
            r#"<ul style="display:none">GONE<li>item</li></ul>"#,
            r#"<ul><p hidden="">GONE</p><li>item</li></ul>"#,
            "<ul>
  <li>item</li>
  <li>item</li>
</ul>",
            "<ul><script>code()</script><li>item</li></ul>",
            "<table><tr><td>cell</td></tr><style>td { color: red }</style></table>",
            r#"<div style="display:none">GONE</div>"#,
            ".x { display: none }",
        ] {
            assert!(!drops_shown(&[".x { display: none }"], body), "{body}");
        }
    }

    #[test]
    fn anydoc_stylesheet_quirks_are_ported() {
        let mut anydoc = AnyDocCascade::default();
        // Its comment stripper closes `/*/` on itself, and its rule split
        // reads the second rule inside an at-rule block.
        anydoc.add(
            "/*/ .a { display: none } */ @media print { .b { color: red } .c { display: none } }",
        );
        let element = |class: &str| {
            Element::new(
                "p",
                vec![("class".to_string(), false, class.to_string())],
                1,
            )
        };
        assert!(anydoc.hides(&element("a")));
        assert!(!anydoc.hides(&element("b")));
        assert!(anydoc.hides(&element("c")));
        // Its selectors must not contain a space, colon, or bracket.
        let mut anydoc = AnyDocCascade::default();
        anydoc
            .add("div .a { display: none } p:first-child { display: none } p[x] { display: none }");
        assert!(!anydoc.hides(&element("a")));
    }

    #[test]
    fn stylesheets_are_parsed_in_linear_time_and_bounded() {
        let started = std::time::Instant::now();
        let unterminated = "@import a#".repeat(64_000);
        assert!(parse_stylesheet(&unterminated)
            .expect("parses")
            .imports
            .is_empty());
        let unclosed = "@import url(".repeat(64_000);
        parse_stylesheet(&unclosed).expect("parses");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "imports must not be rescanned"
        );
        let many = "@import \"a.css\";".repeat(MAX_IMPORTS_PER_SHEET + 1);
        assert!(matches!(
            parse_stylesheet(&many),
            Err(DocumentError::ResourceLimit)
        ));
        let crowded = ".a,".repeat(MAX_STYLESHEET_TOKENS / 2);
        assert!(matches!(
            parse_stylesheet(&crowded),
            Err(DocumentError::ResourceLimit)
        ));
        let deep = format!(
            "{}.x {{ display: none }}{}",
            "@media screen {".repeat(40),
            "}".repeat(40)
        );
        assert!(matches!(
            parse_stylesheet(&deep),
            Err(DocumentError::ResourceLimit)
        ));
    }

    #[test]
    fn matching_work_is_bounded() {
        let (reader, anydoc) = cascade_for(&["[data-x] { visibility: hidden }"]);
        let mut work = MAX_MATCH_WORK;
        assert!(matches!(
            chapter_text(&chapter("<p>TEXT</p>"), &reader, &anydoc, &mut work),
            Err(DocumentError::ResourceLimit)
        ));
    }

    #[test]
    fn external_references_are_read_from_css_syntax() {
        for css in [
            "p { background: url(https://example.com/x.png) }",
            "p { background: URL( \"//example.com/x.png\" ) }",
            "@import \"http://example.com/a.css\";",
            "p { background-image: image-set(\"https://example.com/a.png\" 1x) }",
        ] {
            assert!(references_external(css), "{css}");
        }
        for css in [
            "/* License: MIT (https://opensource.org/licenses/MIT) */ p { margin: 0 }",
            "@namespace epub \"http://www.idpf.org/2007/ops\";",
            "a::after { content: \" (https://example.com)\" }",
            "@font-face { src: url(data:font/woff2;base64,AAAA) }",
            "p { background: url(../images/a.png) }",
        ] {
            assert!(!references_external(css), "{css}");
        }
    }
}

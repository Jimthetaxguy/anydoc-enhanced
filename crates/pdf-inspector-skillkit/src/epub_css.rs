//! Stylesheet evaluation for the EPUB hidden-content check.
//!
//! A chapter's text is flagged when a reading system would hide it and
//! AnyDoc would still convert it. Two models answer those questions.
//!
//! - The reader model follows CSS as reading systems apply it: the CSS
//!   Syntax 3 tokenizer (comments, escapes, strings), media queries, the
//!   selector grammar, the cascade, and the user-agent rules that hide
//!   content. Sibling combinators and positions among siblings are matched
//!   exactly: positions from a pass over the chapter before the walk, `+`
//!   from the earlier siblings the walk keeps, and a rule's `~` step from
//!   the first sibling that fits it, noted as each sibling ends, which
//!   costs a lookup however far back that sibling is. Where it cannot
//!   decide (`:has()`, an unknown pseudo-class, a value set through
//!   `var()`), it lets a hiding rule apply and keeps a showing rule from
//!   overriding one, so it errs toward finding hidden text.
//! - The AnyDoc model ports AnyDoc 0.2.4's own subset (`shared::html`):
//!   `display` from bare `tag`, `.class`, and `tag.class` rules and from
//!   inline styles, applied only to the elements its walker styles.
//!
//! Text that both models hide, or that AnyDoc drops, is not flagged: the
//! conversion then matches what a reader shows.
//!
//! A chapter is also flagged when AnyDoc runs together text a reader shows
//! on separate lines. For that the reader model reads how boxes flow:
//! `display` (inline-level, block-level, or laying children out as flex or
//! grid items), `float` and `position`, and `::before` and `::after` boxes
//! that are blocks or keep a line feed. The chapter walk mirrors AnyDoc's
//! inline runs, including the way it flattens a link's blocks into the text
//! around it. Text runs together where a word, or a number, meets another
//! with nothing between; closing punctuation joins the word before it as
//! written. A break only a rule the walk cannot settle gives counts where
//! digits meet, which the Markdown reads as one number.
//!
//! Text a `::before` or `::after` box shows is text AnyDoc drops: flagged
//! when it holds letters or digits, and for a sign an amount reads by
//! ("−", "(", "%") when it meets digits.

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
/// Style rules that set `display`, `visibility`, `content-visibility`,
/// `float`, or `position`, or style `::before` and `::after` boxes, across a
/// package's stylesheets. Real books carry a few dozen.
pub(super) const MAX_STYLE_RULES: usize = 16_384;
/// Compound-selector evaluations across a package: one element tested
/// against one rule costs one per compound it reaches, and one when the
/// ancestor filter sets the rule aside.
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
    Float,
    /// What a `::before` or `::after` box shows.
    Content,
    /// Whether a box is out of the line's flow (`absolute`, `fixed`).
    Position,
    /// Whether a `::before` or `::after` box keeps the line feeds it shows.
    WhiteSpace,
    /// Whether a `::before` or `::after` box is transparent (`opacity: 0`).
    Opacity,
}

/// What a `content` declaration makes a `::before` or `::after` box show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Generated {
    /// No box (`none`, `normal`).
    Nothing,
    /// A box without text that carries meaning: empty, white space, quote
    /// marks, dashes, bullets, and other ornaments, or an image.
    Plain,
    /// Such a box holding a line feed (`"\A"`), which breaks the line where
    /// the box keeps white space (`white-space: pre`).
    LineFeed,
    /// A sign an amount reads by, which a reader shows beside the digits
    /// after or before it, and otherwise an ornament, such as a hyphen for
    /// a bullet.
    Sign(Sign),
    /// Text a reader shows: letters or digits, a counter, or an attribute's
    /// value.
    Text,
}

/// Where a sign that generated content shows reads as part of an amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sign {
    /// A decimal point or thousands separator ending the box: part of a
    /// number where a digit follows it directly, as "1" and ".99" read
    /// "1.99"; after a number, as in "1.", it only closes it.
    Separator,
    /// A minus or plus, a parenthesis, a percent or currency sign, or a
    /// dash ("−", "(", "%", "$"): beside digits on either side.
    Amount,
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
    /// How the box flows: for `display`, whether it is inline-level; for
    /// `float`, whether it floats; for `position`, whether it leaves the
    /// flow. `Maybe` for a value not known until run time; `None` for a
    /// declaration a reader ignores.
    flow: Option<Tri>,
    /// For `display`, whether the box lays its children out as flex or
    /// grid items, each a block of its own.
    items: Option<Tri>,
    /// For `float`, whether the box floats to the start of the line (`left`
    /// or `inline-start`), where a drop cap stands.
    side: Option<Tri>,
    /// For `content`, what the box shows; `None` when not known until run
    /// time.
    generated: Option<Generated>,
}

/// `display` keywords that lay a box's children out as flex or grid items.
const ITEM_DISPLAY_KEYWORDS: [&str; 14] = [
    "flex",
    "grid",
    "inline-flex",
    "inline-grid",
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
];

/// Signs an amount reads by, which generated content may add to it: a
/// minus or plus, parentheses, a percent sign, currency signs (`¢` to `¥`
/// and the currency block, `€` and `₹` among them), and the hyphens and
/// dashes set as a minus or between figures.
fn amount_sign(character: char) -> bool {
    matches!(
        character,
        '\u{2212}'
            | '-'
            | '+'
            | '('
            | ')'
            | '%'
            | '$'
            | '\u{a2}'..='\u{a5}'
            | '\u{2010}'..='\u{2013}'
            | '\u{20a0}'..='\u{20cf}'
    )
}

/// What a `content` value shows, from its tokens.
fn generated_content(value: &[Token]) -> Option<Generated> {
    let mut generated = Generated::Plain;
    for token in value {
        match token {
            Token::Ident(word) => match word.to_ascii_lowercase().as_str() {
                "none" | "normal" => return Some(Generated::Nothing),
                "initial" | "unset" | "revert" => return Some(Generated::Nothing),
                _ => {}
            },
            Token::Str(text) => {
                if text.chars().any(char::is_alphanumeric) {
                    generated = Generated::Text;
                } else if text.chars().any(amount_sign) {
                    generated = generated.max(Generated::Sign(Sign::Amount));
                } else if text.ends_with(['.', ',']) {
                    generated = generated.max(Generated::Sign(Sign::Separator));
                } else if text.contains('\n') {
                    generated = generated.max(Generated::LineFeed);
                }
            }
            Token::Function(function) => match function.to_ascii_lowercase().as_str() {
                "counter" | "counters" | "attr" => generated = Generated::Text,
                "var" | "env" | "if" => return None,
                _ => {}
            },
            _ => {}
        }
    }
    Some(generated)
}

/// `white-space` and `white-space-collapse` keywords that keep line feeds.
const LINE_FEED_KEYWORDS: [&str; 6] = [
    "pre",
    "pre-wrap",
    "pre-line",
    "break-spaces",
    "preserve",
    "preserve-breaks",
];

/// Keywords that collapse line feeds to spaces.
const COLLAPSING_KEYWORDS: [&str; 5] = ["normal", "nowrap", "collapse", "wrap", "initial"];

/// `display` keywords that make a box inline-level: it sits in the line
/// around it. `initial` and `unset` give `inline`.
const INLINE_DISPLAY_KEYWORDS: [&str; 17] = [
    "inline",
    "inline-block",
    "inline-table",
    "inline-flex",
    "inline-grid",
    "inline-list-item",
    "ruby",
    "ruby-base",
    "ruby-text",
    "contents",
    "math",
    "-webkit-inline-box",
    "-webkit-inline-flex",
    "-moz-inline-box",
    "-ms-inline-flexbox",
    "-ms-inline-grid",
    "initial",
];

/// `display` keywords that make a box block-level, a reader starting a new
/// line for it, or a part of a table, a box of its own beside the others.
const BLOCK_DISPLAY_KEYWORDS: [&str; 20] = [
    "block",
    "flow",
    "flow-root",
    "table",
    "flex",
    "grid",
    "list-item",
    "table-row-group",
    "table-header-group",
    "table-footer-group",
    "table-row",
    "table-cell",
    "table-column-group",
    "table-column",
    "table-caption",
    "-webkit-box",
    "-webkit-flex",
    "-moz-box",
    "-ms-flexbox",
    "-ms-grid",
];

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
/// `content-visibility`, or `float`. A value computed at run time (`var()`,
/// `env()`, `attr()`, `if()`) may hide, so it counts as hiding.
fn parse_declaration(tokens: &[Token]) -> Option<Declaration> {
    let tokens = trim_whitespace(tokens);
    let [Token::Ident(name), rest @ ..] = tokens else {
        return None;
    };
    let property = match name.to_ascii_lowercase().as_str() {
        "display" => Property::Display,
        "visibility" => Property::Visibility,
        "content-visibility" => Property::ContentVisibility,
        "float" => Property::Float,
        "content" => Property::Content,
        "position" => Property::Position,
        "white-space" | "white-space-collapse" => Property::WhiteSpace,
        "opacity" => Property::Opacity,
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
    // `content` has always read an attribute's value (`attr()`), which it
    // shows as text.
    let run_time: &[&str] = match property {
        Property::Content => &["var", "env", "if"],
        _ => &["var", "env", "attr", "if"],
    };
    for token in value {
        match token {
            Token::Ident(word) => keywords.push(word.to_ascii_lowercase()),
            Token::Function(function)
                if run_time
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
    let flow = match property {
        _ if computed => Some(Tri::Maybe),
        Property::Opacity => transparent(value),
        _ if other || keywords.is_empty() => None,
        Property::Display if effect != Effect::Show => None,
        Property::Display if has(&INLINE_DISPLAY_KEYWORDS) || has(&["unset"]) => Some(Tri::Yes),
        Property::Display if has(&BLOCK_DISPLAY_KEYWORDS) => Some(Tri::No),
        Property::Display => Some(Tri::Maybe),
        Property::Float if keywords.len() > 1 => None,
        Property::Float if has(&["left", "right", "inline-start", "inline-end"]) => Some(Tri::Yes),
        Property::Float if has(&["none", "initial", "unset"]) => Some(Tri::No),
        Property::Float => Some(Tri::Maybe),
        Property::Position if keywords.len() > 1 => None,
        Property::Position if has(&["absolute", "fixed"]) => Some(Tri::Yes),
        Property::Position if has(&["static", "relative", "sticky", "initial", "unset"]) => {
            Some(Tri::No)
        }
        Property::Position => None,
        Property::WhiteSpace if has(&LINE_FEED_KEYWORDS) => Some(Tri::Yes),
        Property::WhiteSpace
            if keywords
                .iter()
                .all(|word| COLLAPSING_KEYWORDS.contains(&word.as_str())) =>
        {
            Some(Tri::No)
        }
        // `inherit`, `unset`, and `revert` take the element's value.
        Property::WhiteSpace => Some(Tri::Maybe),
        Property::Visibility | Property::ContentVisibility | Property::Content => None,
    };
    let items = match property {
        Property::Display if computed => Some(Tri::Maybe),
        Property::Display if effect == Effect::Show => Some(if has(&ITEM_DISPLAY_KEYWORDS) {
            Tri::Yes
        } else {
            Tri::No
        }),
        _ => None,
    };
    let side = match (property, flow) {
        (Property::Float, Some(Tri::Yes)) => Some(if has(&["left", "inline-start"]) {
            Tri::Yes
        } else {
            Tri::No
        }),
        (Property::Float, Some(floats)) => Some(floats),
        _ => None,
    };
    let generated = match property {
        Property::Content if computed => None,
        Property::Content => generated_content(value),
        _ => None,
    };
    if property == Property::Content && generated.is_none() && !computed {
        return None;
    }
    Some(Declaration {
        property,
        effect,
        important,
        flow,
        items,
        side,
        generated,
    })
}

/// Whether an `opacity` value makes a box transparent: zero, or less, as a
/// number or a percentage. `inherit` takes the parent's, not read.
fn transparent(value: &[Token]) -> Option<Tri> {
    match value {
        [Token::Numeric(number)] => number
            .trim_end_matches('%')
            .parse::<f64>()
            .ok()
            .map(|opacity| if opacity <= 0.0 { Tri::Yes } else { Tri::No }),
        [Token::Ident(word)] => match word.to_ascii_lowercase().as_str() {
            "inherit" => Some(Tri::Maybe),
            "initial" | "unset" | "revert" | "revert-layer" => Some(Tri::No),
            _ => None,
        },
        _ => None,
    }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Tri {
    #[default]
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
    /// `+`: the element just before.
    Adjacent,
    /// `~`: any element before.
    General,
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
    /// Where the element sits among its siblings, counted from the first
    /// or the last, among all of them or those of its name.
    Nth {
        step: i64,
        offset: i64,
        from_last: bool,
        of_type: bool,
    },
    /// The only one, among all or those of its name.
    Only {
        of_type: bool,
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
    pseudo_element: PseudoElement,
}

/// The pseudo-element a selector styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PseudoElement {
    None,
    /// `::before`, a box before the element's content.
    Before,
    /// `::after`, a box after it.
    After,
    /// `::first-line`, `::marker`, and the rest.
    Other,
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
            pseudo_element: PseudoElement::None,
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
    let mut pseudo_element = PseudoElement::None;
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
            Token::Delim('+') => Some(Combinator::Adjacent),
            Token::Delim('~') => Some(Combinator::General),
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
    pseudo_element: &mut PseudoElement,
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
                *pseudo_element = match tokens.get(index + 2) {
                    Some(Token::Ident(name)) if name.eq_ignore_ascii_case("before") => {
                        PseudoElement::Before
                    }
                    Some(Token::Ident(name)) if name.eq_ignore_ascii_case("after") => {
                        PseudoElement::After
                    }
                    _ => PseudoElement::Other,
                };
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
                        *pseudo_element = match name.as_str() {
                            "before" => PseudoElement::Before,
                            "after" => PseudoElement::After,
                            _ => PseudoElement::Other,
                        };
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
        "first-child" | "last-child" | "first-of-type" | "last-of-type" => PseudoClass::Nth {
            step: 0,
            offset: 1,
            from_last: name.starts_with("last"),
            of_type: name.ends_with("of-type"),
        },
        "only-child" => PseudoClass::Only { of_type: false },
        "only-of-type" => PseudoClass::Only { of_type: true },
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
        "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
            specificity.1 += 1;
            parse_nth(arguments).map_or(PseudoClass::Undecided, |(step, offset)| PseudoClass::Nth {
                step,
                offset,
                from_last: name.starts_with("nth-last"),
                of_type: name.ends_with("of-type"),
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

/// Where an element sits among its parent's element children, counted in
/// a pass over the chapter before the walk.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Position {
    /// From 1, among all of them and among those of its name.
    index: u32,
    index_of_type: u32,
    /// How many there are, and how many of its name; 0 where not counted.
    count: u32,
    count_of_type: u32,
}

/// An element as the selector engine sees it.
pub(super) struct Element {
    /// The local name as written.
    local: String,
    lower: String,
    /// Attribute local names (lowercased), whether each is prefixed, and
    /// decoded values, in document order.
    attributes: Vec<(String, bool, String)>,
    position: Position,
    /// Its element name, ids, and classes as the keys rules are filtered by
    /// (see [`selector_key`]).
    keys: Box<[u64]>,
}

impl Element {
    pub(super) fn new(
        local: &str,
        attributes: Vec<(String, bool, String)>,
        position: Position,
    ) -> Self {
        let mut element = Element {
            local: local.to_string(),
            lower: local.to_ascii_lowercase(),
            attributes,
            position,
            keys: Box::default(),
        };
        element.keys = std::iter::once(selector_key(KEY_TYPE, &element.lower))
            .chain(element.values("id").map(|(_, id)| selector_key(KEY_ID, id)))
            .chain(element.values("class").flat_map(|(_, classes)| {
                classes
                    .split_ascii_whitespace()
                    .map(|class| selector_key(KEY_CLASS, class))
            }))
            .collect();
        element
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

fn nth_matches(step: i64, offset: i64, index: u32) -> bool {
    let index = i64::from(index);
    if step == 0 {
        return index == offset;
    }
    let distance = index - offset;
    distance % step == 0 && distance / step >= 0
}

/// The earlier element siblings of the open element at one depth, as far
/// as selectors test them: the first and the most recent, the names, ids,
/// and classes of any let go between them to bound memory, and those of
/// all, so that a `~` step for a sibling not there costs a lookup. For the
/// `~` steps of the rules, where the first sibling that may fit one came,
/// and the first that certainly does, found as each sibling ends (see
/// [`Cascade::note_sibling`]): that settles the step for every later
/// sibling, however many were let go.
#[derive(Default)]
pub(super) struct Earlier {
    first: Vec<Element>,
    recent: std::collections::VecDeque<Element>,
    dropped: bool,
    dropped_keys: std::collections::HashSet<u64>,
    seen_keys: std::collections::HashSet<u64>,
    /// The siblings taken in so far, kept or let go.
    count: u32,
    /// By step: the places, counted from 0, of the first sibling that may
    /// fit it and of the first that certainly does (`u32::MAX` for none).
    steps: HashMap<u32, (u32, u32)>,
}

/// Earlier siblings kept at each depth: the first, which rules such as
/// `h1 ~ p` test, and the most recent, which `h2 + p` tests.
const FIRST_SIBLINGS_KEPT: usize = 32;
const RECENT_SIBLINGS_KEPT: usize = 96;

impl Earlier {
    fn push(&mut self, element: Element) {
        self.count += 1;
        self.seen_keys.extend(AncestorKeys::keys(&element));
        if self.first.len() < FIRST_SIBLINGS_KEPT {
            self.first.push(element);
            return;
        }
        if self.recent.len() == RECENT_SIBLINGS_KEPT {
            if let Some(gone) = self.recent.pop_front() {
                self.dropped_keys.extend(AncestorKeys::keys(&gone));
            }
            self.dropped = true;
        }
        self.recent.push_back(element);
    }

    fn len(&self) -> usize {
        self.first.len() + self.recent.len()
    }

    fn get(&self, at: usize) -> &Element {
        match at.checked_sub(self.first.len()) {
            None => &self.first[at],
            Some(recent) => &self.recent[recent],
        }
    }

    /// Whether siblings were let go before the kept one at `at`.
    fn gap_before(&self, at: usize) -> bool {
        self.dropped && at >= self.first.len()
    }

    /// Where a kept sibling sits among all taken in, counted from 0; an
    /// open element (`None`) comes after them all.
    fn place(&self, sibling: Option<usize>) -> u32 {
        match sibling {
            None => self.count,
            Some(at) if at < self.first.len() => at as u32,
            Some(at) => self.count - self.recent.len() as u32 + (at - self.first.len()) as u32,
        }
    }

    /// Whether a sibling before `place` fits a `~` step.
    fn step_before(&self, step: u32, place: u32) -> Tri {
        match self.steps.get(&step) {
            Some(&(_, certain)) if certain < place => Tri::Yes,
            Some(&(possible, _)) if possible < place => Tri::Maybe,
            _ => Tri::No,
        }
    }

    /// Whether a sibling taken in certainly fits a `~` step, which settles
    /// it for all the siblings after.
    fn step_settled(&self, step: u32) -> bool {
        self.steps
            .get(&step)
            .is_some_and(|&(_, certain)| certain != u32::MAX)
    }

    /// Note how the sibling at `place` fits a `~` step.
    fn note_step(&mut self, step: u32, fits: Tri, place: u32) {
        let (possible, certain) = self.steps.entry(step).or_insert((u32::MAX, u32::MAX));
        if fits != Tri::No {
            *possible = (*possible).min(place);
        }
        if fits == Tri::Yes {
            *certain = (*certain).min(place);
        }
    }
}

/// The elements selectors test: the open elements, root first, and the
/// earlier siblings of each.
pub(super) struct Tree<'a> {
    stack: &'a [Element],
    earlier: &'a [Earlier],
}

/// An open element, or one of its earlier siblings (`sibling`, counted
/// among those kept).
#[derive(Clone, Copy)]
struct Node {
    depth: usize,
    sibling: Option<usize>,
}

/// The element just before another among its siblings.
enum Before {
    Element(Node),
    Nothing,
    /// It was let go to bound memory.
    Unknown,
}

impl<'a> Tree<'a> {
    fn element(&self, node: Node) -> &'a Element {
        match node.sibling {
            None => &self.stack[node.depth],
            Some(at) => self.earlier[node.depth].get(at),
        }
    }

    fn top(&self) -> Node {
        Node {
            depth: self.stack.len() - 1,
            sibling: None,
        }
    }

    fn parent(&self, node: Node) -> Option<Node> {
        node.depth.checked_sub(1).map(|depth| Node {
            depth,
            sibling: None,
        })
    }

    /// The kept earlier siblings of a node, nearest first.
    fn earlier(&self, node: Node) -> impl Iterator<Item = Node> {
        let at = node
            .sibling
            .unwrap_or_else(|| self.earlier[node.depth].len());
        (0..at).rev().map(move |sibling| Node {
            depth: node.depth,
            sibling: Some(sibling),
        })
    }

    fn before(&self, node: Node) -> Before {
        let earlier = &self.earlier[node.depth];
        let at = node.sibling.unwrap_or_else(|| earlier.len());
        if at == 0 {
            Before::Nothing
        } else if earlier.gap_before(at) && at == earlier.first.len() {
            Before::Unknown
        } else {
            Before::Element(Node {
                depth: node.depth,
                sibling: Some(at - 1),
            })
        }
    }

    /// Whether some earlier sibling carries each element name, id, and
    /// class a compound requires.
    fn siblings_may_fit(&self, node: Node, compound: &Compound) -> bool {
        let seen = &self.earlier[node.depth].seen_keys;
        compound_keys(compound).all(|key| seen.contains(&key))
    }

    /// Whether an earlier sibling of a node let go might fit a compound:
    /// one carried each element name, id, and class the compound requires.
    fn gap_may_fit(&self, node: Node, compound: &Compound) -> bool {
        let earlier = &self.earlier[node.depth];
        earlier.gap_before(node.sibling.unwrap_or_else(|| earlier.len()))
            && compound_keys(compound).all(|key| earlier.dropped_keys.contains(&key))
    }

    /// Whether an earlier sibling of a node fits a `~` step the walk
    /// records, with the part of the selector before it.
    fn step_before(&self, node: Node, step: u32) -> Tri {
        let earlier = &self.earlier[node.depth];
        earlier.step_before(step, earlier.place(node.sibling))
    }
}

fn match_simple(simple: &Simple, tree: &Tree, node: Node, work: &mut u64) -> Tri {
    let element = tree.element(node);
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
                if node.depth == 0 {
                    Tri::Yes
                } else {
                    Tri::No
                }
            }
            PseudoClass::Nth {
                step,
                offset,
                from_last,
                of_type,
            } => {
                let position = element.position;
                let (index, count) = match of_type {
                    false => (position.index, position.count),
                    true => (position.index_of_type, position.count_of_type),
                };
                let index = match from_last {
                    false => Some(index),
                    true => (count > 0).then(|| count + 1 - index),
                };
                match index {
                    Some(index) if nth_matches(*step, *offset, index) => Tri::Yes,
                    Some(_) => Tri::No,
                    None => Tri::Maybe,
                }
            }
            PseudoClass::Only { of_type } => {
                let count = match of_type {
                    false => element.position.count,
                    true => element.position.count_of_type,
                };
                match count {
                    0 => Tri::Maybe,
                    1 => Tri::Yes,
                    _ => Tri::No,
                }
            }
            PseudoClass::Not(list) => list
                .iter()
                .map(|selector| match_complex_at(selector, &[], tree, node, work))
                .max()
                .unwrap_or(Tri::No)
                .not(),
            PseudoClass::Is(list) => list
                .iter()
                .map(|selector| match_complex_at(selector, &[], tree, node, work))
                .max()
                .unwrap_or(Tri::No),
            PseudoClass::Undecided => Tri::Maybe,
        },
        Simple::Unknown => Tri::Maybe,
    }
}

fn match_compound(compound: &Compound, tree: &Tree, node: Node, work: &mut u64) -> Tri {
    *work += 1;
    let mut result = Tri::Yes;
    for simple in &compound.parts {
        result = result.min(match_simple(simple, tree, node, work));
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

/// Whether a selector's compounds up to `last` match at `node`, walking
/// them right to left. `recorded` holds, by combinator, the ids of the
/// rule's `~` steps the walk records (see [`Earlier`]); a selector inside
/// a pseudo-class has none.
fn match_chain(
    selector: &ComplexSelector,
    recorded: &[Option<u32>],
    last: usize,
    tree: &Tree,
    node: Node,
    mode: MatchMode,
    work: &mut u64,
) -> bool {
    mode.accepts(match_compound(&selector.compounds[last], tree, node, work))
        && match_before(selector, recorded, tree, node, last, mode, work)
}

impl MatchMode {
    fn accepts(self, tri: Tri) -> bool {
        match self {
            MatchMode::Possible => tri != Tri::No,
            MatchMode::Certain => tri == Tri::Yes,
        }
    }
}

/// Match the compounds before `step`, compound `step` having matched at
/// `node`, trying each element a descendant or `~` step may take until one
/// leads to a match. A `~` step the walk records is settled by the first
/// sibling that fits it. Where another reaches siblings let go to bound
/// memory, or the work runs out, the match is possible but not certain.
fn match_before(
    selector: &ComplexSelector,
    recorded: &[Option<u32>],
    tree: &Tree,
    node: Node,
    step: usize,
    mode: MatchMode,
    work: &mut u64,
) -> bool {
    let Some(previous) = step.checked_sub(1) else {
        return true;
    };
    let undecided = mode == MatchMode::Possible;
    if *work > MAX_MATCH_WORK {
        return undecided;
    }
    let compound = &selector.compounds[previous];
    let fits = |candidate: Node, work: &mut u64| {
        mode.accepts(match_compound(compound, tree, candidate, work))
            && match_before(selector, recorded, tree, candidate, previous, mode, work)
    };
    match selector.combinators[previous] {
        Combinator::Child => tree.parent(node).is_some_and(|parent| fits(parent, work)),
        Combinator::Descendant => {
            // Where only descendant steps are left, the nearest ancestor
            // that fits settles it: a farther one has fewer ancestors.
            let nearest_settles = selector.combinators[..previous]
                .iter()
                .all(|combinator| *combinator == Combinator::Descendant);
            let mut candidate = tree.parent(node);
            while let Some(ancestor) = candidate {
                if mode.accepts(match_compound(compound, tree, ancestor, work)) {
                    if match_before(selector, recorded, tree, ancestor, previous, mode, work) {
                        return true;
                    }
                    if nearest_settles {
                        return false;
                    }
                }
                if *work > MAX_MATCH_WORK {
                    return undecided;
                }
                candidate = tree.parent(ancestor);
            }
            false
        }
        Combinator::Adjacent => match tree.before(node) {
            Before::Element(before) => fits(before, work),
            Before::Nothing => false,
            Before::Unknown => undecided,
        },
        Combinator::General => {
            if let Some(&Some(id)) = recorded.get(previous) {
                return mode.accepts(tree.step_before(node, id));
            }
            if !tree.siblings_may_fit(node, compound) {
                return false;
            }
            for sibling in tree.earlier(node) {
                if fits(sibling, work) {
                    return true;
                }
                if *work > MAX_MATCH_WORK {
                    return undecided;
                }
            }
            undecided && tree.gap_may_fit(node, compound)
        }
    }
}

/// How a selector's compounds up to `last` match at `node`: certainly,
/// possibly, or not. The certain match, which settles most, comes first.
fn match_prefix_at(
    selector: &ComplexSelector,
    recorded: &[Option<u32>],
    last: usize,
    tree: &Tree,
    node: Node,
    work: &mut u64,
) -> Tri {
    if !match_chain(
        selector,
        recorded,
        last,
        tree,
        node,
        MatchMode::Possible,
        work,
    ) {
        Tri::No
    } else if match_chain(
        selector,
        recorded,
        last,
        tree,
        node,
        MatchMode::Certain,
        work,
    ) {
        Tri::Yes
    } else {
        Tri::Maybe
    }
}

fn match_complex_at(
    selector: &ComplexSelector,
    recorded: &[Option<u32>],
    tree: &Tree,
    node: Node,
    work: &mut u64,
) -> Tri {
    let last = selector.compounds.len() - 1;
    match_prefix_at(selector, recorded, last, tree, node, work)
}

/// How a selector matches the element at the top of the tree.
fn match_complex(
    selector: &ComplexSelector,
    recorded: &[Option<u32>],
    tree: &Tree,
    work: &mut u64,
) -> Tri {
    match_complex_at(selector, recorded, tree, tree.top(), work)
}

// ---------------------------------------------------------------------------
// Stylesheets

/// A style rule that sets a property the check reads.
#[derive(Debug)]
struct StyleRule {
    selector: ComplexSelector,
    declarations: Rc<[Declaration]>,
    /// The classes, ids, and element names its ancestor compounds require
    /// (see [`AncestorKeys`]).
    ancestor_keys: Box<[u64]>,
}

/// A class, id, or element name, hashed without regard to ASCII case. Two
/// names that collide only let more rules through to the full match.
fn selector_key(kind: u8, name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut hasher);
    for byte in name.bytes() {
        byte.to_ascii_lowercase().hash(&mut hasher);
    }
    hasher.finish()
}

const KEY_TYPE: u8 = 0;
const KEY_ID: u8 = 1;
const KEY_CLASS: u8 = 2;

/// The keys the ancestor compounds of a selector's compounds up to `last`
/// require: those joined to the compound on their right by a descendant or
/// child combinator, whose element names, ids, and classes some ancestor
/// must carry. A compound before a sibling combinator matches a sibling,
/// and requires nothing of the ancestors.
fn ancestor_keys(selector: &ComplexSelector, last: usize) -> Box<[u64]> {
    let mut keys: Vec<u64> = Vec::new();
    for (compound, combinator) in selector.compounds[..last].iter().zip(&selector.combinators) {
        if matches!(combinator, Combinator::Adjacent | Combinator::General) {
            continue;
        }
        for key in compound_keys(compound) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys.into_boxed_slice()
}

/// The element name, ids, and classes an element must carry to fit a
/// compound.
fn compound_keys(compound: &Compound) -> impl Iterator<Item = u64> + '_ {
    compound.parts.iter().filter_map(|part| match part {
        Simple::Type {
            name: Some(name), ..
        } => Some(selector_key(KEY_TYPE, name)),
        Simple::Id(id) => Some(selector_key(KEY_ID, id)),
        Simple::Class(class) => Some(selector_key(KEY_CLASS, class)),
        _ => None,
    })
}

/// The element names, ids, and classes of the open elements, counted, so
/// that a rule for another part of a book is set aside with a lookup per
/// key, as browsers filter rules by their ancestors.
#[derive(Default)]
pub(super) struct AncestorKeys {
    counts: HashMap<u64, u32>,
}

impl AncestorKeys {
    fn keys(element: &Element) -> impl Iterator<Item = u64> + '_ {
        element.keys.iter().copied()
    }

    fn push(&mut self, element: &Element) {
        for key in Self::keys(element) {
            *self.counts.entry(key).or_default() += 1;
        }
    }

    fn pop(&mut self, element: &Element) {
        for key in Self::keys(element) {
            if let Some(count) = self.counts.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    self.counts.remove(&key);
                }
            }
        }
    }

    /// Whether some open element carries each key.
    fn hold(&self, keys: &[u64]) -> bool {
        keys.iter().all(|key| self.counts.contains_key(key))
    }
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
        // A `::before` or `::after` box matters for where a reader breaks
        // lines, which its `display`, `content`, `float`, `position`, and
        // `white-space` decide, and for the text it shows, which its
        // `visibility` and `opacity` may keep unseen. An element's own
        // `content`, `white-space`, and `opacity` are not read.
        let lays_out = declarations
            .iter()
            .any(|declaration| !matches!(declaration.property, Property::ContentVisibility));
        let styles_element = declarations.iter().any(|declaration| {
            !matches!(
                declaration.property,
                Property::Content | Property::WhiteSpace | Property::Opacity
            )
        });
        for selector in parse_selector_list(selectors, 0) {
            let kept = match selector.pseudo_element {
                PseudoElement::None => styles_element,
                PseudoElement::Before | PseudoElement::After => lays_out,
                PseudoElement::Other => false,
            };
            if kept {
                let ancestor_keys = ancestor_keys(&selector, selector.compounds.len() - 1);
                sheet.rules.push(Rc::new(StyleRule {
                    selector,
                    declarations: declarations.clone(),
                    ancestor_keys,
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
    /// Whether the box is inline-level.
    inline: Tri,
    /// Whether it floats beside the lines around it, and whether to their
    /// start, where a drop cap stands.
    floats: Tri,
    floats_to_start: Tri,
    /// Whether it is positioned out of the flow (`absolute`, `fixed`),
    /// apart from the lines around it.
    positioned: Tri,
    /// Whether it lays its children out as flex or grid items.
    items: Tri,
    /// Its `::before` and `::after` boxes.
    before: PseudoBox,
    after: PseudoBox,
}

/// What a `::before` or `::after` box does, as far as the check reads it.
#[derive(Clone, Copy, Default)]
struct PseudoBox {
    /// It is a block in the flow, or keeps a line feed, and breaks the line
    /// before or after the element's content.
    breaks: Tri,
    /// It certainly shows text that carries meaning, which AnyDoc does not
    /// convert.
    text: bool,
    /// A sign an amount reads by that it certainly shows, which matters
    /// beside a digit.
    sign: Option<Sign>,
    /// Whether its text is certainly unseen (`visibility: hidden`,
    /// `opacity: 0`); `None` where it takes the element's visibility.
    unseen: Option<bool>,
}

/// How a reader lays an element out beside the text around it, as far as
/// its style can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    /// In the line.
    Inline,
    /// A block of its own, starting a new line.
    Block,
    /// Floated beside the lines after it.
    Float,
    /// Positioned out of the flow, where its style sets it.
    Positioned,
    /// A block, floated, or positioned only if a rule that may apply does.
    MaybeApart,
}

impl ReaderStyle {
    fn flow(&self) -> Flow {
        match (self.inline, self.floats, self.positioned) {
            // A positioned box leaves the flow, floated or not.
            (_, _, Tri::Yes) => Flow::Positioned,
            (_, Tri::Yes, _) => Flow::Float,
            (Tri::No, Tri::No, Tri::No) => Flow::Block,
            (Tri::Yes, Tri::No, Tri::No) => Flow::Inline,
            _ => Flow::MaybeApart,
        }
    }
}

/// The value of the certain declaration that wins the cascade, or `default`
/// when none does, and whether one that may apply above it says otherwise.
fn resolve_value<T: Copy + PartialEq>(applied: &[(Precedence, Tri, T)], default: T) -> (T, bool) {
    let best = applied
        .iter()
        .filter(|(_, certainty, _)| *certainty == Tri::Yes)
        .max_by_key(|(precedence, _, _)| *precedence);
    let value = best.map_or(default, |best| best.2);
    let contested = applied.iter().any(|(precedence, certainty, other)| {
        *certainty != Tri::Yes && best.is_none_or(|best| *precedence > best.0) && *other != value
    });
    (value, contested)
}

/// Whether all of three may hold: `Yes` if each certainly does, `No` if one
/// certainly does not.
fn all_three(first: Tri, second: Tri, third: Tri) -> Tri {
    if [first, second, third].contains(&Tri::No) {
        Tri::No
    } else if [first, second, third].iter().all(|tri| *tri == Tri::Yes) {
        Tri::Yes
    } else {
        Tri::Maybe
    }
}

/// Resolve how a box flows from the declarations that may apply: the value
/// of the certain one that wins the cascade, or `default` when none does,
/// unless one that may apply above it says otherwise.
fn resolve_flow(applied: &[(Precedence, Tri, Tri)], default: bool) -> Tri {
    let best = applied
        .iter()
        .filter(|(_, certainty, _)| *certainty == Tri::Yes)
        .max_by_key(|(precedence, _, _)| *precedence);
    let value = best.map_or(if default { Tri::Yes } else { Tri::No }, |best| best.2);
    let contested = applied.iter().any(|(precedence, certainty, other)| {
        *certainty != Tri::Yes && best.is_none_or(|best| *precedence > best.0) && *other != value
    });
    if contested {
        Tri::Maybe
    } else {
        value
    }
}

/// Elements a reader shows as boxes of their own by default (HTML's
/// rendering section and MathML's): AnyDoc's blocks and containers, block
/// elements it does not know, the parts of a table, and a display formula.
fn reader_block_by_default(element: &Element) -> bool {
    let local = element.lower.as_str();
    anydoc_block(local)
        || matches!(
            local,
            "address"
                | "dialog"
                | "fieldset"
                | "form"
                | "hgroup"
                | "legend"
                | "listing"
                | "menu"
                | "dir"
                | "plaintext"
                | "search"
                | "xmp"
                | "caption"
                | "thead"
                | "tbody"
                | "tfoot"
                | "tr"
                | "td"
                | "th"
        )
        || (local == "math"
            && element
                .first("display")
                .is_some_and(|display| display.eq_ignore_ascii_case("block")))
}

enum RuleKey {
    Id(String),
    Class(String),
    Tag(String),
}

/// The id, class, or element name a compound requires, the rarest kind
/// first, by which an element that cannot fit it is set aside.
fn rarest_key(compound: &Compound) -> Option<u64> {
    let key = |kind: u8| {
        compound.parts.iter().find_map(|part| match (part, kind) {
            (Simple::Id(id), KEY_ID) => Some(selector_key(KEY_ID, id)),
            (Simple::Class(class), KEY_CLASS) => Some(selector_key(KEY_CLASS, class)),
            (
                Simple::Type {
                    name: Some(name), ..
                },
                KEY_TYPE,
            ) => Some(selector_key(KEY_TYPE, name)),
            _ => None,
        })
    };
    key(KEY_ID)
        .or_else(|| key(KEY_CLASS))
        .or_else(|| key(KEY_TYPE))
}

/// The rules a chapter applies in cascade order, indexed by the id, class,
/// or element name their rightmost compound requires.
#[derive(Default)]
pub(super) struct Cascade {
    rules: Vec<CascadeRule>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
    /// Rules for `::before` and `::after` boxes.
    pseudo_rules: usize,
    /// The `~` steps of the rules, which the walk records as siblings end.
    sibling_steps: Vec<SiblingStep>,
    /// Those steps by an id, class, or element name their compound
    /// requires, and those that require none.
    steps_by_key: HashMap<u64, Vec<u32>>,
    steps_anywhere: Vec<u32>,
}

/// A rule in a chapter's cascade: its place in the cascade order, and the
/// ids of its `~` steps by combinator.
struct CascadeRule {
    rule: Rc<StyleRule>,
    order: u32,
    recorded: Box<[Option<u32>]>,
}

/// A `~` step of a rule: the rule, the compound before the combinator, and
/// the keys the part of the selector up to it requires of the ancestors,
/// which a sibling that fits it shares.
struct SiblingStep {
    rule: u32,
    compound: u32,
    ancestor_keys: Box<[u64]>,
}

impl Cascade {
    pub(super) fn push_sheet(&mut self, sheet: &Stylesheet) {
        for rule in &sheet.rules {
            let index = self.rules.len();
            let recorded = rule
                .selector
                .combinators
                .iter()
                .enumerate()
                .map(|(at, combinator)| {
                    (*combinator == Combinator::General).then(|| {
                        let step = self.sibling_steps.len() as u32;
                        self.sibling_steps.push(SiblingStep {
                            rule: index as u32,
                            compound: at as u32,
                            ancestor_keys: ancestor_keys(&rule.selector, at),
                        });
                        match rarest_key(&rule.selector.compounds[at]) {
                            Some(key) => self.steps_by_key.entry(key).or_default().push(step),
                            None => self.steps_anywhere.push(step),
                        }
                        step
                    })
                })
                .collect();
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
            if rule.selector.pseudo_element != PseudoElement::None {
                self.pseudo_rules += 1;
            }
            match key {
                Some(RuleKey::Id(id)) => self.by_id.entry(id).or_default().push(index),
                Some(RuleKey::Class(class)) => self.by_class.entry(class).or_default().push(index),
                Some(RuleKey::Tag(tag)) => self.by_tag.entry(tag).or_default().push(index),
                None => self.universal.push(index),
            }
            self.rules.push(CascadeRule {
                rule: rule.clone(),
                order: index as u32 + 1,
                recorded,
            });
        }
    }

    pub(super) fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Record how the element that has just ended among the siblings at
    /// the top of `earlier` fits each `~` step of the rules, with the part
    /// of the selector before it: its siblings before it, and its
    /// ancestors, are those a later sibling's match would reach through
    /// it. A step one sibling certainly fits is settled for all after it,
    /// and not tried again; one whose ancestors the open elements lack is
    /// set aside with a lookup per key.
    fn note_sibling(
        &self,
        stack: &[Element],
        earlier: &mut [Earlier],
        ancestors: &AncestorKeys,
        work: &mut u64,
    ) {
        if self.sibling_steps.is_empty() || *work > MAX_MATCH_WORK {
            return;
        }
        let depth = stack.len();
        let fits: Vec<(u32, Tri)> = {
            let tree = Tree {
                stack,
                earlier: &*earlier,
            };
            let siblings = &tree.earlier[depth];
            let node = Node {
                depth,
                sibling: Some(siblings.len() - 1),
            };
            let keyed = AncestorKeys::keys(tree.element(node))
                .filter_map(|key| self.steps_by_key.get(&key))
                .flatten();
            let mut fits = Vec::new();
            for &step in self.steps_anywhere.iter().chain(keyed) {
                if siblings.step_settled(step) {
                    continue;
                }
                if *work > MAX_MATCH_WORK {
                    break;
                }
                let sibling_step = &self.sibling_steps[step as usize];
                if !ancestors.hold(&sibling_step.ancestor_keys) {
                    *work += 1;
                    continue;
                }
                let CascadeRule { rule, recorded, .. } = &self.rules[sibling_step.rule as usize];
                let fit = match_prefix_at(
                    &rule.selector,
                    recorded,
                    sibling_step.compound as usize,
                    &tree,
                    node,
                    work,
                );
                if fit != Tri::No {
                    fits.push((step, fit));
                }
            }
            fits
        };
        let siblings = &mut earlier[depth];
        let place = siblings.count - 1;
        for (step, fit) in fits {
            siblings.note_step(step, fit, place);
        }
    }

    /// Whether any rule styles a `::before` or `::after` box, which may
    /// break lines around an element with no content of its own.
    fn styles_pseudo_boxes(&self) -> bool {
        self.pseudo_rules > 0
    }

    /// The cascade for the element at the top of `stack`: author rules,
    /// its inline style, SVG presentation attributes, and the user-agent
    /// rules that hide content.
    fn evaluate(
        &self,
        tree: &Tree,
        ancestors: &AncestorKeys,
        work: &mut u64,
    ) -> Result<ReaderStyle, DocumentError> {
        let element = tree.stack.last().expect("an element to style");
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
        // Whether the box is inline-level, floats, lays its children out as
        // items, is positioned out of the flow, and floats to the start of
        // the line.
        let mut flows: [Vec<(Precedence, Tri, Tri)>; 5] = Default::default();
        let mut add = |declaration: &Declaration, precedence: Precedence, certainty: Tri| {
            let slot = match declaration.property {
                Property::Display => 0,
                Property::Visibility => 1,
                Property::ContentVisibility => 2,
                Property::Float | Property::Position => {
                    let flow_slot = match declaration.property {
                        Property::Float => 1,
                        _ => 3,
                    };
                    if let Some(flow) = declaration.flow {
                        flows[flow_slot].push((precedence, certainty, flow));
                    }
                    if let Some(side) = declaration.side {
                        flows[4].push((precedence, certainty, side));
                    }
                    return;
                }
                // What only `::before` and `::after` boxes read.
                Property::Content | Property::WhiteSpace | Property::Opacity => return,
            };
            if let (Property::Display, Some(flow)) = (declaration.property, declaration.flow) {
                flows[0].push((precedence, certainty, flow));
            }
            if let (Property::Display, Some(items)) = (declaration.property, declaration.items) {
                flows[2].push((precedence, certainty, items));
            }
            applied[slot].push(Applied {
                precedence,
                certainty,
                effect: declaration.effect,
            });
        };
        // For each of `::before` and `::after`: whether it is a block box,
        // whether `display: none` removes it, what it shows, whether it
        // leaves the flow by position or float, whether it keeps line
        // feeds, whether `visibility` hides it (`Maybe` for a value not
        // known until run time, `None` to take the element's), and whether
        // it is transparent.
        let mut pseudo_blocks: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_none: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_content: [Vec<(Precedence, Tri, Option<Generated>)>; 2] = Default::default();
        let mut pseudo_out: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_floats: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_line_feeds: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_hidden: [Vec<(Precedence, Tri, Option<Tri>)>; 2] = Default::default();
        let mut pseudo_clear: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        for index in candidates {
            let CascadeRule {
                rule,
                order,
                recorded,
            } = &self.rules[index];
            if !ancestors.hold(&rule.ancestor_keys) {
                *work += 1;
                continue;
            }
            let certainty = match_complex(&rule.selector, recorded, tree, work);
            if certainty == Tri::No {
                continue;
            }
            let pseudo = match rule.selector.pseudo_element {
                PseudoElement::None => None,
                PseudoElement::Before => Some(0),
                PseudoElement::After => Some(1),
                PseudoElement::Other => continue,
            };
            if let Some(slot) = pseudo {
                for declaration in rule.declarations.iter() {
                    let tier = if declaration.important {
                        TIER_AUTHOR_IMPORTANT
                    } else {
                        TIER_AUTHOR
                    };
                    let precedence = Precedence {
                        tier,
                        specificity: rule.selector.specificity,
                        order: *order,
                    };
                    match (declaration.property, declaration.flow) {
                        (Property::Content, _) => {
                            pseudo_content[slot].push((
                                precedence,
                                certainty,
                                declaration.generated,
                            ));
                            continue;
                        }
                        (Property::Position, Some(out)) => {
                            pseudo_out[slot].push((precedence, certainty, out));
                            continue;
                        }
                        (Property::Float, Some(floats)) => {
                            pseudo_floats[slot].push((precedence, certainty, floats));
                            continue;
                        }
                        (Property::WhiteSpace, Some(kept)) => {
                            pseudo_line_feeds[slot].push((precedence, certainty, kept));
                            continue;
                        }
                        (Property::Visibility, computed) => {
                            let hidden = match declaration.effect {
                                _ if computed.is_some() => Some(Tri::Maybe),
                                Effect::Hide => Some(Tri::Yes),
                                Effect::Show => Some(Tri::No),
                                Effect::Inherit => None,
                                Effect::Neutral => continue,
                            };
                            pseudo_hidden[slot].push((precedence, certainty, hidden));
                            continue;
                        }
                        (Property::Opacity, Some(clear)) => {
                            pseudo_clear[slot].push((precedence, certainty, clear));
                            continue;
                        }
                        _ => {}
                    }
                    let none = match (declaration.property, declaration.effect, declaration.flow) {
                        (Property::Display, Effect::Hide, None) => Some(Tri::Yes),
                        // A value computed at run time.
                        (Property::Display, Effect::Hide, Some(_)) => Some(Tri::Maybe),
                        (Property::Display, Effect::Show, _) => Some(Tri::No),
                        _ => None,
                    };
                    if let Some(none) = none {
                        pseudo_none[slot].push((precedence, certainty, none));
                    }
                    let block = match (declaration.property, declaration.flow) {
                        (Property::Display, Some(Tri::No)) => Tri::Yes,
                        (Property::Display, Some(Tri::Yes)) => Tri::No,
                        (Property::Display, Some(Tri::Maybe)) => Tri::Maybe,
                        (Property::Display, None) if declaration.effect == Effect::Hide => Tri::No,
                        _ => continue,
                    };
                    pseudo_blocks[slot].push((precedence, certainty, block));
                }
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
                    flow: None,
                    items: None,
                    side: None,
                    generated: None,
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
                    flow: None,
                    items: None,
                    side: None,
                    generated: None,
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
                    flow: None,
                    items: None,
                    side: None,
                    generated: None,
                },
                user_agent,
                Tri::Yes,
            );
        }
        // A box exists where `content` gives one and `display` does not take
        // it away: what it shows, and whether it breaks the line, as a block
        // in the flow or with a line feed it keeps. Without a `white-space`
        // of its own, it keeps line feeds as the element does, which is not
        // read. A floated or positioned box leaves the line. Its text is
        // unseen only where `opacity: 0` or `visibility` certainly keeps
        // it so.
        let pseudo = |slot: usize| {
            let (content, contested) =
                resolve_value(&pseudo_content[slot], Some(Generated::Nothing));
            let given = match content {
                Some(Generated::Nothing) if !contested => Tri::No,
                Some(_) if !contested => Tri::Yes,
                _ => Tri::Maybe,
            };
            let exists = all_three(
                given,
                resolve_flow(&pseudo_none[slot], false).not(),
                Tri::Yes,
            );
            let certain = !contested && exists == Tri::Yes;
            let text = certain && content == Some(Generated::Text);
            let sign = match content {
                Some(Generated::Sign(sign)) if certain => Some(sign),
                _ => None,
            };
            let line_feed = match content {
                Some(Generated::LineFeed) if !contested => {
                    match resolve_value(&pseudo_line_feeds[slot], Tri::Maybe) {
                        (_, true) => Tri::Maybe,
                        (kept, false) => kept,
                    }
                }
                _ => Tri::No,
            };
            let out_of_flow = resolve_flow(&pseudo_out[slot], false)
                .max(resolve_flow(&pseudo_floats[slot], false));
            let breaks = all_three(
                exists,
                resolve_flow(&pseudo_blocks[slot], false).max(line_feed),
                out_of_flow.not(),
            );
            let unseen = if resolve_flow(&pseudo_clear[slot], false) == Tri::Yes {
                Some(true)
            } else {
                match resolve_value(&pseudo_hidden[slot], None) {
                    (Some(Tri::Yes), false) => Some(true),
                    (None, false) => None,
                    _ => Some(false),
                }
            };
            PseudoBox {
                breaks,
                text,
                sign,
                unseen,
            }
        };
        let (before, after) = (pseudo(0), pseudo(1));
        Ok(ReaderStyle {
            display: resolve(&applied[0]),
            visibility: resolve(&applied[1]),
            content_visibility: resolve(&applied[2]),
            inline: resolve_flow(&flows[0], !reader_block_by_default(element)),
            floats: resolve_flow(&flows[1], false),
            floats_to_start: resolve_flow(&flows[4], false),
            positioned: resolve_flow(&flows[3], false),
            items: resolve_flow(&flows[2], false),
            before,
            after,
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
    /// Children sit in an SVG image, outside a `foreignObject`.
    in_svg: bool,
    /// A reader paints nothing inside where it stands (see
    /// [`svg_unpainted`]).
    unpainted: bool,
    /// Inside an SVG `text` element, whose text a reader paints; other SVG
    /// elements paint none right inside them.
    svg_text: bool,
    /// For an SVG `switch`: whether a child it may render has come, which
    /// leaves the later ones unrendered.
    switch_taken: Option<bool>,
    /// Children are laid out as flex or grid items.
    items: Tri,
    exempt: Exempt,
    /// What the element does to AnyDoc's inline run as it ends.
    effects: Effects,
}

/// What an element does to AnyDoc's inline run (see [`Run`]) as it ends.
#[derive(Clone, Copy, Default)]
struct Effects {
    /// It opened a run of its own.
    opens_run: bool,
    /// AnyDoc starts a new paragraph after it.
    flush_after: bool,
    /// A reader starts a new line after it and AnyDoc does not: `Maybe`
    /// where only a rule that may apply says so.
    boundary_after: Tri,
    /// It opened a [`Splice`].
    closes_splice: bool,
    /// It is a block inside a link (see [`Run::edge`]).
    edge_after: bool,
    /// A reader floats or positions it: where it began, to tell a drop
    /// cap, beside which the text after it continues.
    glyphs_at: Option<FloatStart>,
    /// Its `::after` box shows a sign, lost beside a digit: the one the text
    /// before it ends in, or the one the text after it starts with.
    sign_after: Option<Sign>,
}

/// Where a floated or positioned box began: the glyphs and digits taken in
/// before it, and whether it is a float to the line's start that opens its
/// paragraph.
#[derive(Clone, Copy)]
struct FloatStart {
    glyphs: (u64, u64),
    opens: bool,
}

/// One inline run of AnyDoc's walker (a `Builder`): the text it last
/// added, and whether a reader starts a new block since then without AnyDoc
/// starting a new paragraph. Text added across such a boundary with no white
/// space between runs together in the Markdown, as "Balance due1,250.00".
#[derive(Default)]
struct Run {
    last: Option<char>,
    /// The character before `last`, where both came from text.
    before_last: Option<char>,
    boundary: bool,
    /// A reader starts a new line here only if a rule that may apply says
    /// so, or a float sits here: it counts where digits meet, which the
    /// Markdown then reads as one number.
    maybe_boundary: bool,
    /// The links, and the lists, tables, and quotes inside them, whose
    /// content the run is taking in, innermost last.
    splices: Vec<Splice>,
    /// The alt text of the packaged image the run last took in, while no
    /// other text follows it.
    alt: Option<String>,
    /// A floated drop cap ended, and the next text starts beside it.
    beside_float: Option<DropCap>,
    /// A sign a `::before` or `::after` box shows sits before the next
    /// text.
    sign_before: Option<Sign>,
    /// A generated sign met a digit, which AnyDoc then shows without it.
    lost_sign: bool,
}

/// A floated drop cap, beside which the next text continues its word: any
/// text after one or two characters, and after three, as an InDesign drop
/// cap of a word's first letters, only text going on in lower case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropCap {
    Any,
    Lowercase,
}

/// A link's content, which AnyDoc flattens into the run around the link
/// (`inline_children_at`): the blocks inside it joined by line breaks, the
/// first and last with nothing between them and the text on either side,
/// and each list, table, or quote inside it reduced to its text with its
/// items, cells, and blocks joined by spaces (`block_text`). A reader shows
/// every one of those blocks on lines of its own.
struct Splice {
    /// What AnyDoc writes between two parts that keep content: a line break
    /// between a link's blocks, a space within a list, table, or quote.
    separator: char,
    /// A part before the current one kept content.
    parts: bool,
    /// The current part keeps content.
    kept: bool,
    /// Leading white space of the current part is dropped (AnyDoc's
    /// `start_boundary`, set at every block edge).
    trim: bool,
    /// White space or a line break waiting in the current part, kept only
    /// if content follows before the part ends.
    pending: Option<char>,
    /// Text AnyDoc keeps whole below a `pre` or `math` element.
    whole: Whole,
}

/// How a part of a link's content takes in the text below a `pre` or
/// `math` element, which AnyDoc keeps whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Whole {
    /// None of it: only walked text joins the run.
    No,
    /// `pre` text as it stands (`elem.text()`).
    Pre,
    /// A display formula's TeX (`mathml_to_tex`), which leaves out the
    /// white space between its tokens.
    Tex,
}

impl Run {
    fn flush(&mut self) {
        *self = Run::default();
    }

    /// Whether AnyDoc drops the leading white space of text added now
    /// (`at_space_boundary`).
    fn at_space(&self) -> bool {
        match self.splices.last() {
            Some(splice) if !splice.kept => splice.trim || splice.pending.is_some(),
            _ => self.last.is_none_or(char::is_whitespace),
        }
    }

    /// Start taking in a link's content, or a list, table, quote, `pre`, or
    /// display formula inside one.
    fn open_splice(&mut self, separator: char, whole: Whole) {
        let trim = match separator {
            '\n' => self.at_space(),
            _ => whole != Whole::Pre,
        };
        self.splices.push(Splice {
            separator,
            parts: false,
            kept: false,
            trim,
            pending: None,
            whole,
        });
    }

    /// One of AnyDoc's blocks starts or ends inside a link: the part ends,
    /// and white space waiting in it is dropped. Whether a reader starts a
    /// new line there is the element's own layout.
    fn edge(&mut self) {
        if let Some(splice) = self.splices.last_mut() {
            splice.parts |= splice.kept;
            splice.kept = false;
            splice.trim = true;
            splice.pending = None;
        }
    }

    /// Content arrives: in a link, each part it starts is first given its
    /// separator, or the white space waiting in it.
    fn content(&mut self) {
        let mut gap = None;
        for splice in &mut self.splices {
            if !splice.kept {
                if splice.parts {
                    gap = Some(splice.separator);
                } else if let Some(pending) = splice.pending {
                    gap = gap.or(Some(pending));
                }
                splice.kept = true;
                splice.pending = None;
            }
        }
        if gap.is_some() {
            self.last = gap;
        }
    }

    /// White space, or a line break (`br`), with no content beside it.
    fn space(&mut self, character: char) {
        match self.splices.last_mut() {
            Some(splice) if !splice.kept => {
                if character == '\n' {
                    splice.pending = Some('\n');
                } else if !(splice.trim || splice.pending.is_some()) {
                    splice.pending = Some(' ');
                }
            }
            _ => {
                if self.last.is_some() {
                    self.last = Some(character);
                }
            }
        }
    }

    /// Note where a reader may start a new line.
    fn mark(&mut self, boundary: Tri) {
        match boundary {
            Tri::Yes => self.boundary = true,
            Tri::Maybe => self.maybe_boundary = true,
            Tri::No => {}
        }
    }

    /// Add text to the run; whether it runs into the text before it.
    fn add(&mut self, text: &str) -> bool {
        self.take(text, false)
    }

    /// Take in a packaged image's alt text.
    fn add_alt(&mut self, alt: &str) -> bool {
        let fused = self.take(alt, true);
        self.alt = Some(alt.to_string()).filter(|alt| !alt.is_empty());
        fused
    }

    /// Add text, or alt text, to the run; whether it runs into the text
    /// before it where a reader starts a new line. Alt text is not text a
    /// reader shows: where it meets other text, as pandoc 2 figures run an
    /// image's caption into it ("ChartChart"), only digits on both sides,
    /// which read as one number, count as joined.
    fn take(&mut self, text: &str, alt: bool) -> bool {
        let kept = || text.chars().filter(|character| anydoc_keeps(*character));
        let Some(last) = kept().next_back() else {
            return false;
        };
        if kept().all(char::is_whitespace) {
            self.space(' ');
            if self.sign_before == Some(Sign::Separator) {
                self.sign_before = None;
            }
            return false;
        }
        let at_space = self.at_space();
        let mut from_first = kept().skip_while(|character| at_space && character.is_whitespace());
        let (first, second) = (from_first.next(), from_first.next());
        // A separator joins only the digits right after it; another sign
        // reads with the amount after the white space too.
        let meets_sign = match self.sign_before.take() {
            Some(Sign::Separator) => kept().next().is_some_and(char::is_numeric),
            Some(Sign::Amount) => kept()
                .find(|character| !character.is_whitespace())
                .is_some_and(char::is_numeric),
            None => false,
        };
        self.lost_sign |= meets_sign;
        self.content();
        let (joined, number) = meeting([self.before_last, self.last], [first, second]);
        let beside = match self.beside_float {
            Some(DropCap::Any) => true,
            Some(DropCap::Lowercase) => first.is_some_and(char::is_lowercase),
            None => false,
        };
        let alt_side = (alt || self.alt.is_some()) && !number;
        let fused =
            joined && (self.boundary || (self.maybe_boundary && number)) && !beside && !alt_side;
        self.before_last = kept().rev().nth(1).or(self.last);
        self.last = Some(last);
        self.boundary = false;
        self.maybe_boundary = false;
        self.beside_float = None;
        self.alt = None;
        fused
    }
}

/// How text reads where it meets the text before it with nothing between:
/// whether it runs into it, and whether two numbers run together, digits
/// meeting directly or across a decimal point or thousands separator ("12."
/// and "5" reading "12.5"). Text that opens with closing punctuation, as a
/// period set on a line of its own, joins the word before it as written.
fn meeting(before: [Option<char>; 2], after: [Option<char>; 2]) -> (bool, bool) {
    let [before_last, last] = before;
    let [first, second] = after;
    let (Some(last), Some(first)) = (last, first) else {
        return (false, false);
    };
    let separator = |character: char| matches!(character, '.' | ',');
    let number = (last.is_numeric() && first.is_numeric())
        || (last.is_numeric() && separator(first) && second.is_some_and(char::is_numeric))
        || (separator(last) && first.is_numeric() && before_last.is_some_and(char::is_numeric));
    let closing = matches!(
        first,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{201d}' | '\u{2019}' | '\u{bb}'
    );
    let joined = !last.is_whitespace() && !first.is_whitespace() && (!closing || number);
    (joined, number)
}

/// Whether AnyDoc keeps a character of text: `clean_text` drops soft hyphens,
/// zero-width spaces, byte order marks, and control characters other than
/// tabs and line ends.
fn anydoc_keeps(character: char) -> bool {
    !matches!(character, '\u{ad}' | '\u{200b}' | '\u{feff}')
        && (!character.is_control() || matches!(character, '\t' | '\n' | '\r'))
}

/// Whether AnyDoc converts a `math` element as a display formula, a block
/// of its own (`mathml_is_display`).
fn anydoc_display_math(element: &Element) -> bool {
    element.first("display") == Some("block")
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

/// What the pass before the walk finds for each element of a chapter.
#[derive(Clone, Copy, Default)]
struct Facts {
    /// A child element is one of AnyDoc's blocks (`has_block_children`).
    has_blocks: bool,
    position: Position,
}

/// What the pass before the walk finds in a chapter: the facts of each
/// element in document order, and the ids something refers to where that
/// makes a reader paint an SVG resource (see [`svg_unpainted`]).
#[derive(Default)]
struct ChapterFacts {
    elements: Vec<Facts>,
    /// The ids an SVG `use` element draws.
    used: std::collections::HashSet<String>,
    /// The ids a fill, stroke, clip path, mask, or marker paints
    /// (`url(#id)`), in an attribute or a `style` element.
    painted: std::collections::HashSet<String>,
}

/// Properties that paint the SVG resource they refer to: a pattern as a
/// fill or stroke, a clip path, a mask, and markers.
const PAINTING_PROPERTIES: [&str; 8] = [
    "fill",
    "stroke",
    "clip-path",
    "mask",
    "marker",
    "marker-start",
    "marker-mid",
    "marker-end",
];

/// The ids the `url(#id)` references in a value refer to.
fn url_fragments(value: &str) -> impl Iterator<Item = &str> + '_ {
    let lower = value.to_ascii_lowercase();
    let starts: Vec<usize> = lower.match_indices("url(").map(|(at, _)| at + 4).collect();
    starts.into_iter().filter_map(move |start| {
        let rest = value[start..].trim_start().trim_start_matches(['"', '\'']);
        let id = rest.strip_prefix('#')?;
        let end = id
            .find(|character: char| {
                matches!(character, ')' | '"' | '\'') || character.is_whitespace()
            })
            .unwrap_or(id.len());
        Some(&id[..end])
    })
}

/// Add the ids that painting declarations in a `style` attribute or
/// element refer to.
fn painted_by_style(css: &str, painted: &mut std::collections::HashSet<String>) {
    for declaration in css.split([';', '{', '}']) {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if PAINTING_PROPERTIES
            .iter()
            .any(|property| name.eq_ignore_ascii_case(property))
        {
            painted.extend(url_fragments(value).map(str::to_string));
        }
    }
}

/// An open element in the pass before the walk, or the chapter's top
/// level: its element children so far, each with the name it carries, and
/// how many carry each name; whether its children sit in an SVG image, and
/// whether it is a `style` element, whose text is a stylesheet.
#[derive(Default)]
struct Family {
    fact: Option<usize>,
    children: Vec<(u32, u32)>,
    names: HashMap<String, u32>,
    counts: Vec<u32>,
    svg: bool,
    style: bool,
}

impl Family {
    /// Give each child the counts its siblings make.
    fn finish(self, facts: &mut [Facts]) {
        let count = self.children.len() as u32;
        for (fact, name) in self.children {
            let position = &mut facts[fact as usize].position;
            position.count = count;
            position.count_of_type = self.counts[name as usize];
        }
    }
}

/// For each element of a chapter in document order: whether a child is one
/// of AnyDoc's blocks, and where it sits among its siblings; and the ids
/// that make a reader paint an SVG resource.
fn element_facts(chapter: &[u8]) -> Result<ChapterFacts, DocumentError> {
    let mut xml = quick_xml::Reader::from_reader(std::io::Cursor::new(chapter));
    xml.config_mut().trim_text(false);
    xml.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut found = ChapterFacts::default();
    let facts = &mut found.elements;
    let mut open: Vec<Family> = vec![Family::default()];
    loop {
        let event = xml
            .read_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                if open.len() > 1 {
                    open.pop().expect("an open element").finish(facts);
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Text(text)
                if open.last().is_some_and(|family| family.style) =>
            {
                painted_by_style(&String::from_utf8_lossy(text.as_ref()), &mut found.painted);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::CData(text)
                if open.last().is_some_and(|family| family.style) =>
            {
                painted_by_style(&String::from_utf8_lossy(text.as_ref()), &mut found.painted);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => {
                while let Some(family) = open.pop() {
                    family.finish(facts);
                }
                return Ok(found);
            }
            _ => {
                buffer.clear();
                continue;
            }
        };
        let name = element.name();
        let local = String::from_utf8_lossy(super::xml_local_name(name.as_ref())).into_owned();
        let family = open.last_mut().expect("the chapter's top level");
        let svg = family.svg || local == "svg";
        for attribute in element.attributes().flatten() {
            let key = attribute.key.as_ref();
            if key == b"xmlns" || key.starts_with(b"xmlns:") {
                continue;
            }
            let key = super::xml_local_name(key);
            let used = key == b"href" && svg && local == "use";
            let styled = key.eq_ignore_ascii_case(b"style");
            let painting = PAINTING_PROPERTIES
                .iter()
                .any(|property| key.eq_ignore_ascii_case(property.as_bytes()));
            if !(used || styled || painting) {
                continue;
            }
            let value = attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(attribute.value.as_ref()).into_owned());
            if used {
                if let Some(id) = value.trim().strip_prefix('#') {
                    found.used.insert(id.to_string());
                }
            } else if styled {
                painted_by_style(&value, &mut found.painted);
            } else {
                found
                    .painted
                    .extend(url_fragments(&value).map(str::to_string));
            }
        }
        if let Some(parent) = family.fact {
            facts[parent].has_blocks |= anydoc_block(&local);
        }
        let style = local == "style";
        let foreign = local == "foreignObject";
        let next = family.names.len() as u32;
        let name = *family.names.entry(local).or_insert(next);
        if name as usize == family.counts.len() {
            family.counts.push(0);
        }
        family.counts[name as usize] += 1;
        family.children.push((facts.len() as u32, name));
        facts.push(Facts {
            has_blocks: false,
            position: Position {
                index: family.children.len() as u32,
                index_of_type: family.counts[name as usize],
                count: 0,
                count_of_type: 0,
            },
        });
        if start {
            open.push(Family {
                fact: Some(facts.len() - 1),
                svg: svg && !foreign,
                style,
                ..Family::default()
            });
        }
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

/// SVG elements a reader paints what they hold where they stand: the image,
/// its groups and links, a `switch`, text and the spans and paths in it,
/// and a `foreignObject`, whose HTML it lays out. Of these, only text and
/// the HTML paint the text right inside them.
fn svg_paints(local: &str) -> bool {
    matches!(
        local,
        "svg" | "g" | "a" | "switch" | "text" | "tspan" | "textPath" | "foreignObject"
    )
}

/// Whether a reader paints nothing an element inside an SVG image holds,
/// where it stands. A resource paints only where something refers to it:
/// a `symbol`, or an element inside `defs`, where a `use` element draws it,
/// and a pattern, clip path, mask, or marker where a fill, stroke, clip,
/// mask, or marker names it. Gradients, filters, and elements SVG does not
/// define paint nothing.
fn svg_unpainted(element: &Element, facts: &ChapterFacts, parent_unpainted: bool) -> bool {
    let local = element.local.as_str();
    let referenced = element.first("id").is_some_and(|id| match local {
        "pattern" | "clipPath" | "mask" | "marker" => facts.painted.contains(id),
        _ => (svg_paints(local) || local == "symbol") && facts.used.contains(id),
    });
    !referenced && (parent_unpainted || !svg_paints(local))
}

/// Whether a reader renders a child of an SVG `switch`, which renders only
/// the first child whose conditions hold: no extension an EPUB names but
/// HTML's holds, and a language holds only on a reader set to it.
fn svg_switch_renders(element: &Element) -> Tri {
    if element
        .first("requiredextensions")
        .is_some_and(|extensions| extensions.trim() != "http://www.w3.org/1999/xhtml")
    {
        Tri::No
    } else if element.first("systemlanguage").is_some() {
        Tri::Maybe
    } else {
        Tri::Yes
    }
}

/// Characters a floated box may hold and still be a drop cap, beside which
/// the text after it continues the word ("O" and "nce upon a time"), and
/// the most when the word goes on in lower case ("Onc" and "e the office").
const MAX_DROP_CAP_LETTERS: u64 = 2;
const MAX_DROP_CAP_WORD_LETTERS: u64 = 3;

/// The characters of text AnyDoc keeps that are not white space, and the
/// digits among them.
fn glyph_count(text: &str) -> (u64, u64) {
    text.chars()
        .filter(|character| anydoc_keeps(*character) && !character.is_whitespace())
        .fold((0, 0), |(glyphs, digits), character| {
            (glyphs + 1, digits + u64::from(character.is_numeric()))
        })
}

/// Add the glyphs of `text` to the count so far.
fn count_glyphs(glyphs: &mut (u64, u64), text: &str) {
    let (characters, digits) = glyph_count(text);
    glyphs.0 += characters;
    glyphs.1 += digits;
}

/// The drop cap a floated box that took in the glyphs between `start` and
/// `end` makes. A digit reads as a number beside the next, so one holding
/// a digit is a drop cap only as a single figure floated to the line's
/// start to open its paragraph, as a year's first ("1" and "914 began");
/// floated to the end, it is a line number in the margin.
fn drop_cap(start: FloatStart, end: (u64, u64)) -> Option<DropCap> {
    let (characters, digits) = (end.0 - start.glyphs.0, end.1 - start.glyphs.1);
    if digits > 0 {
        (start.opens && characters == 1).then_some(DropCap::Any)
    } else if characters <= MAX_DROP_CAP_LETTERS {
        Some(DropCap::Any)
    } else if characters <= MAX_DROP_CAP_WORD_LETTERS {
        Some(DropCap::Lowercase)
    } else {
        None
    }
}

/// Take in an image AnyDoc converts: a packaged image as its alt text,
/// inline, and an image from outside the package as Markdown image markup,
/// which keeps the text on either side apart, except inside a list, table,
/// or quote in a link, which AnyDoc reduces to plain text, alt text and all.
/// Whether the alt text runs into the text before it.
fn take_image(run: &mut Run, element: &Element, glyphs: &mut (u64, u64)) -> bool {
    let source = element
        .first("src")
        .or_else(|| element.first("href"))
        .unwrap_or("");
    let alt = element.first("alt").unwrap_or("").trim();
    let plain = run
        .splices
        .last()
        .is_some_and(|splice| splice.separator == ' ');
    run.content();
    if anydoc_absolute_uri(source) && !plain {
        if run.last.is_some() {
            run.last = Some(' ');
        }
        run.alt = None;
        false
    } else {
        count_glyphs(glyphs, alt);
        run.add_alt(alt)
    }
}

/// An element as it starts, for [`meet_run`].
struct Meeting<'a> {
    parent: Reach,
    reach: Reach,
    element: &'a Element,
    /// A child element is one of AnyDoc's blocks.
    has_blocks: bool,
    /// AnyDoc's styles hide the element, and it skips it as though absent.
    anydoc_hidden: bool,
    /// The reader's style, where it was read.
    style: Option<&'a ReaderStyle>,
    /// The parent lays its children out as flex or grid items.
    parent_items: Tri,
    /// The element is inside an SVG image.
    in_svg: bool,
}

/// How an element meets the run it sits in as it starts, and what it will
/// do to the run as it ends.
fn meet_run(
    run: &mut Run,
    meeting: &Meeting,
    glyphs: &mut (u64, u64),
    found: &mut ChapterText,
) -> Effects {
    let mut effects = Effects::default();
    let Meeting {
        parent,
        reach,
        element,
        has_blocks,
        anydoc_hidden,
        style,
        parent_items,
        in_svg,
    } = *meeting;
    let local = element.local.as_str();
    let spliced = !run.splices.is_empty();
    // How a reader lays the element out beside the text around it. A flex
    // or grid item is a block of its own, as is each text of an SVG image
    // and each span of one set at a place of its own; otherwise its style
    // decides, or, where it was not read (the element holds no text), the
    // reader's defaults.
    let own = match style {
        Some(style) => style.flow(),
        None if reader_block_by_default(element) => Flow::Block,
        None => Flow::Inline,
    };
    let placed = |name: &str| element.first(name).is_some();
    let flow = match (parent_items, own) {
        (Tri::Yes, _) | (_, Flow::Block) => Flow::Block,
        _ if in_svg && (local == "text" || (local == "tspan" && (placed("x") || placed("y")))) => {
            Flow::Block
        }
        // Moved off the line only by a shift that may be a superscript's.
        _ if in_svg && local == "tspan" && placed("dy") => Flow::MaybeApart,
        (Tri::Maybe, _) => Flow::MaybeApart,
        _ => own,
    };
    let (breaks_before, breaks_after) = style.map_or((Tri::No, Tri::No), |style| {
        (style.before.breaks, style.after.breaks)
    });
    let keep_in_run = |run: &mut Run, effects: &mut Effects, glyphs: (u64, u64)| match flow {
        // A block `::before` or `::after` box still breaks the line.
        Flow::Inline => {
            run.mark(breaks_before);
            effects.boundary_after = breaks_after;
        }
        Flow::Block => {
            run.boundary = true;
            effects.boundary_after = Tri::Yes;
        }
        // A float, or a positioned box, starts no line of its own before
        // it; a number beside it stays apart from one in it.
        Flow::Float | Flow::Positioned => {
            let to_start = style.is_some_and(|style| style.floats_to_start == Tri::Yes);
            effects.glyphs_at = Some(FloatStart {
                glyphs,
                opens: flow == Flow::Float && to_start && run.last.is_none(),
            });
            run.maybe_boundary = true;
            effects.boundary_after = Tri::Yes;
        }
        Flow::MaybeApart => {
            run.maybe_boundary = true;
            effects.boundary_after = Tri::Maybe;
        }
    };
    match (parent, reach) {
        // An item, caption, or cell of a list or table inside a link: AnyDoc
        // joins its text to the next with a space.
        (Reach::List | Reach::Table | Reach::Row, Reach::Walk) if spliced => {
            run.edge();
            keep_in_run(run, &mut effects, *glyphs);
            effects.edge_after = true;
        }
        (Reach::Walk, _) if anydoc_hidden => {
            // AnyDoc skips an element its styles hide as though it were not
            // there, while a reader may show it: a line break, or a block,
            // even an empty one, starts a new line.
            let shown = style.is_some_and(|style| style.display != Resolved::Hidden);
            if shown && local == "br" {
                run.boundary = true;
            } else if shown {
                keep_in_run(run, &mut effects, *glyphs);
            }
        }
        (Reach::Walk, _) => match (local, reach) {
            ("a", Reach::Walk) => {
                keep_in_run(run, &mut effects, *glyphs);
                run.open_splice('\n', Whole::No);
                effects.closes_splice = true;
            }
            ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6", Reach::Walk) if spliced => {
                run.edge();
                keep_in_run(run, &mut effects, *glyphs);
                effects.edge_after = true;
            }
            ("blockquote", Reach::Walk) | (_, Reach::List | Reach::Table) if spliced => {
                run.edge();
                keep_in_run(run, &mut effects, *glyphs);
                run.open_splice(' ', Whole::No);
                effects.closes_splice = true;
                effects.edge_after = true;
            }
            ("pre", Reach::Whole) if spliced => {
                run.edge();
                keep_in_run(run, &mut effects, *glyphs);
                run.open_splice(' ', Whole::Pre);
                effects.closes_splice = true;
                effects.edge_after = true;
            }
            // A display formula is one of the link's blocks, its TeX joined
            // to the text on either side.
            ("math", Reach::Whole) if spliced && anydoc_display_math(element) => {
                run.edge();
                keep_in_run(run, &mut effects, *glyphs);
                run.open_splice(' ', Whole::Tex);
                effects.closes_splice = true;
                effects.edge_after = true;
            }
            // An inline formula converts as TeX between dollar signs, apart
            // from the text around it.
            (_, Reach::Whole) if spliced => {
                run.content();
                run.last = Some(' ');
            }
            ("hr", _) if spliced => {
                run.edge();
                keep_in_run(run, &mut effects, *glyphs);
                run.content();
                effects.edge_after = true;
            }
            ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote", Reach::Walk) => {
                run.flush();
                effects.opens_run = true;
                effects.flush_after = true;
            }
            (_, Reach::List | Reach::Table | Reach::Whole) | ("hr", _) => {
                run.flush();
                effects.flush_after = true;
            }
            // A reader breaks the line even where AnyDoc drops the break: a
            // link holding nothing else loses it with its empty paragraph.
            ("br", Reach::Dropped) => {
                run.space('\n');
                if style.is_none_or(|style| style.display != Resolved::Hidden) {
                    run.boundary = true;
                }
            }
            ("img" | "image", Reach::Dropped) => {
                keep_in_run(run, &mut effects, *glyphs);
                found.fuses_blocks |= take_image(run, element, glyphs);
                found.drops_shown |= std::mem::take(&mut run.lost_sign);
            }
            (container, Reach::Walk) if anydoc_container(container) && has_blocks => {
                if spliced {
                    run.edge();
                    keep_in_run(run, &mut effects, *glyphs);
                    effects.edge_after = true;
                } else {
                    run.flush();
                    effects.flush_after = true;
                }
            }
            // Walked inline.
            (_, Reach::Walk) => keep_in_run(run, &mut effects, *glyphs),
            _ => {}
        },
        _ => {}
    }
    effects
}

/// Apply what an element does to AnyDoc's inline run as it ends, with the
/// glyphs taken in so far; whether a sign its `::after` box shows meets the
/// digit the text before it ends in, which AnyDoc then shows without it.
fn end_element(effects: &Effects, runs: &mut Vec<Run>, glyphs: (u64, u64)) -> bool {
    let lost_sign = effects.sign_after == Some(Sign::Amount)
        && runs
            .last()
            .is_some_and(|run| run.last.is_some_and(char::is_numeric));
    if effects.opens_run {
        runs.pop();
    }
    let Some(run) = runs.last_mut() else {
        return lost_sign;
    };
    if effects.closes_splice {
        run.splices.pop();
    }
    if effects.edge_after {
        run.edge();
    }
    if effects.flush_after {
        run.flush();
    }
    match effects.glyphs_at.and_then(|start| drop_cap(start, glyphs)) {
        Some(cap) => run.beside_float = Some(cap),
        None => run.mark(effects.boundary_after),
    }
    // Otherwise the sign sits before the text that goes on in the line,
    // unless the element ends a paragraph or a block of its own.
    let in_line = !effects.opens_run && !effects.flush_after && effects.boundary_after != Tri::Yes;
    if !lost_sign && in_line {
        run.sign_before = run.sign_before.max(effects.sign_after);
    }
    lost_sign
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
    // The earlier siblings of each open element, and of the next at the
    // top: one more than the open elements.
    let mut earlier: Vec<Earlier> = vec![Earlier::default()];
    let mut ancestors = AncestorKeys::default();
    let mut open: Vec<Open> = Vec::new();
    let mut root_taken = false;
    let mut body_taken = false;
    let mut found = ChapterText::default();
    let facts = element_facts(chapter)?;
    let mut element_index = 0usize;
    let mut runs: Vec<Run> = Vec::new();
    // Glyphs, and digits among them, taken into runs so far, to tell a
    // floated drop cap.
    let mut glyphs = (0u64, 0u64);
    loop {
        let event = xml
            .read_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let (start, text) = match event {
            quick_xml::events::Event::Start(start) => (Some((start, true)), None),
            quick_xml::events::Event::Empty(start) => (Some((start, false)), None),
            quick_xml::events::Event::End(_) => {
                if let Some(closed) = elements.pop() {
                    ancestors.pop(&closed);
                    earlier.pop();
                    if let Some(siblings) = earlier.last_mut() {
                        siblings.push(closed);
                        reader.note_sibling(&elements, &mut earlier, &ancestors, work);
                    }
                }
                if let Some(closed) = open.pop() {
                    found.drops_shown |= end_element(&closed.effects, &mut runs, glyphs);
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
            if let (Some(state), Some(run)) = (open.last(), runs.last_mut()) {
                // `pre` text, or a display formula's, inside a link joins the
                // run around it.
                let whole = run.splices.last().map_or(Whole::No, |splice| splice.whole);
                let taken = match (state.reach, whole) {
                    (Reach::Walk, _) | (Reach::Whole, Whole::Pre) => Some(text.as_str()),
                    (Reach::Whole, Whole::Tex) => Some(text.trim()),
                    _ => None,
                };
                if let Some(taken) = taken {
                    count_glyphs(&mut glyphs, taken);
                    found.fuses_blocks |= run.add(taken);
                    found.drops_shown |= std::mem::take(&mut run.lost_sign);
                }
            }
            let state = open
                .last()
                .filter(|_| text.chars().any(|character| !character.is_whitespace()));
            if let Some(state) = state {
                let hidden = state.undisplayed
                    || state.invisible
                    || state.contents_hidden
                    || state.fallback
                    || state.unpainted
                    || (state.in_svg && !state.svg_text);
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
        let fact = facts
            .elements
            .get(element_index)
            .copied()
            .unwrap_or_default();
        let has_blocks = fact.has_blocks;
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
        let element = Element::new(&local, attributes, fact.position);
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
        let parent_reach = open.last().map(|parent| parent.reach);
        // AnyDoc skips an element its styles hide as though it were absent.
        let anydoc_hidden = parent_reach == Some(Reach::Walk)
            && matches!(reach, Reach::Omitted | Reach::Dropped)
            && anydoc.hides(&element);
        ancestors.push(&element);
        elements.push(element);
        earlier.push(Earlier::default());
        let element = elements.last().expect("the element just pushed");
        // The reader's style: for the text below the element, for whether a
        // reader shows an element AnyDoc skips, a line break, or an image,
        // and how it lays them out, and for the `::before` and `::after`
        // boxes of an empty element. Nothing below a dropped or empty
        // element converts or shows.
        let style = if (has_children && reach != Reach::Dropped)
            || (anydoc_hidden && (reach == Reach::Omitted || matches!(local.as_str(), "br" | "hr")))
            || (parent_reach == Some(Reach::Walk)
                && matches!(local.as_str(), "br" | "img" | "image"))
            || (reach != Reach::Dropped && reader.styles_pseudo_boxes())
        {
            let tree = Tree {
                stack: &elements,
                earlier: &earlier,
            };
            Some(reader.evaluate(&tree, &ancestors, work)?)
        } else {
            None
        };
        // How the element meets AnyDoc's inline run. Paragraphs, headings,
        // quotes, list items, cells, captions, and the body get runs of
        // their own; lists, tables, `pre`, rules, and containers holding
        // blocks end the current paragraph; everything else is walked
        // inline, although a reader starts a new line for a container, and
        // for anything its style makes a block. A link's content joins the
        // run around it (see [`Splice`]).
        let spliced = runs.last().is_some_and(|run| !run.splices.is_empty());
        let parent_items = open.last().map_or(Tri::No, |parent| parent.items);
        let parent_in_svg = open.last().is_some_and(|parent| parent.in_svg);
        let mut effects = match (parent_reach, reach, runs.last_mut()) {
            (Some(Reach::Root), Reach::Walk, _) => Effects {
                opens_run: true,
                ..Effects::default()
            },
            (Some(Reach::List | Reach::Table | Reach::Row), Reach::Walk, _) if !spliced => {
                Effects {
                    opens_run: true,
                    ..Effects::default()
                }
            }
            (Some(parent), _, Some(run)) => meet_run(
                run,
                &Meeting {
                    parent,
                    reach,
                    element,
                    has_blocks,
                    anydoc_hidden,
                    style: style.as_ref(),
                    parent_items,
                    in_svg: parent_in_svg,
                },
                &mut glyphs,
                &mut found,
            ),
            _ => Effects::default(),
        };
        let element = elements.last().expect("the element just pushed");
        // Whether a reader paints what the element holds where it stands:
        // inside an SVG image, see [`svg_unpainted`]; of a `switch`'s
        // children, one it renders at most.
        let rendered = match open
            .last_mut()
            .and_then(|parent| parent.switch_taken.as_mut())
        {
            Some(taken) => {
                let renders = if *taken {
                    Tri::No
                } else {
                    svg_switch_renders(element)
                };
                *taken |= renders != Tri::No;
                renders
            }
            None => Tri::Yes,
        };
        let parent = open.last();
        let in_svg = parent_in_svg || element.lower == "svg";
        let parent_unpainted = parent.is_some_and(|parent| parent.unpainted);
        let unpainted = if parent_in_svg {
            svg_unpainted(element, &facts, parent_unpainted) || rendered != Tri::Yes
        } else {
            parent_unpainted
        };
        let svg_text = parent.is_some_and(|parent| parent.svg_text)
            || (parent_in_svg && element.local == "text");
        let switch_taken = (in_svg && element.local == "switch").then_some(false);
        // A `foreignObject` holds HTML, laid out as a reader lays out a page.
        let children_in_svg = in_svg && element.local != "foreignObject";
        let mut state = match &style {
            // Nothing below converts or shows, so its style does not matter.
            None => Open {
                reach,
                caption_seen: false,
                undisplayed: false,
                invisible: false,
                contents_hidden: false,
                fallback: false,
                in_svg: children_in_svg,
                unpainted,
                svg_text,
                switch_taken,
                items: Tri::No,
                exempt: Exempt::None,
                effects,
            },
            Some(style) => {
                let inherited_invisible = parent.is_some_and(|parent| parent.invisible);
                let exempt = match parent.map(|parent| parent.exempt) {
                    Some(exempt) if exempt != Exempt::None => exempt,
                    _ if in_svg
                        && matches!(element.lower.as_str(), "title" | "desc" | "metadata") =>
                    {
                        Exempt::Description
                    }
                    _ if element.lower == "rp" => Exempt::RubyParenthesis,
                    _ => Exempt::None,
                };
                Open {
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
                    in_svg: children_in_svg,
                    unpainted,
                    svg_text,
                    switch_taken,
                    items: style.items,
                    exempt,
                    effects,
                }
            }
        };
        // What a shown `::before` or `::after` box adds, which AnyDoc never
        // converts: text, such as a label, and a sign beside digits. A box
        // takes the element's visibility unless its own settles it. AnyDoc
        // writes a list item with a list marker of its own, which stands in
        // for a sign before or after it.
        let hidden = state.undisplayed || state.contents_hidden || state.unpainted;
        let generated = style
            .as_ref()
            .filter(|_| reach != Reach::Dropped && !hidden && !in_svg && !replaced(&element.lower));
        let seen = |pseudo: &PseudoBox| !pseudo.unseen.unwrap_or(state.invisible);
        found.drops_shown |= generated.is_some_and(|style| {
            [style.before, style.after]
                .iter()
                .any(|pseudo| pseudo.text && seen(pseudo))
        });
        let list_item = parent_reach == Some(Reach::List) && reach == Reach::Walk && !spliced;
        let signs = generated.filter(|_| !list_item);
        let sign_before = signs.and_then(|style| style.before.sign.filter(|_| seen(&style.before)));
        effects.sign_after =
            signs.and_then(|style| style.after.sign.filter(|_| seen(&style.after)));
        state.effects.sign_after = effects.sign_after;
        // A sign before the element's content sits before the text after
        // it, and after the text the line holds before it.
        let mark_sign = |runs: &mut Vec<Run>| {
            let Some(run) = runs.last_mut().filter(|_| sign_before.is_some()) else {
                return false;
            };
            run.sign_before = run.sign_before.max(sign_before);
            sign_before == Some(Sign::Amount)
                && !run.boundary
                && run.last.is_some_and(char::is_numeric)
        };
        // An empty element holds no text to check.
        if !has_children {
            if let Some(closed) = elements.pop() {
                ancestors.pop(&closed);
                earlier.pop();
                if let Some(siblings) = earlier.last_mut() {
                    siblings.push(closed);
                    reader.note_sibling(&elements, &mut earlier, &ancestors, work);
                }
            }
            effects.opens_run = false;
            found.drops_shown |= mark_sign(&mut runs);
            found.drops_shown |= end_element(&effects, &mut runs, glyphs);
            buffer.clear();
            continue;
        }
        if effects.opens_run {
            runs.push(Run::default());
        }
        found.drops_shown |= mark_sign(&mut runs);
        open.push(state);
        buffer.clear();
    }
}

/// Elements a reader replaces with other content, which shows no `::before`
/// or `::after` box.
fn replaced(local: &str) -> bool {
    matches!(
        local,
        "input"
            | "select"
            | "textarea"
            | "iframe"
            | "object"
            | "embed"
            | "video"
            | "audio"
            | "canvas"
            | "img"
            | "image"
            | "br"
            | "hr"
    )
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
            // Siblings and positions among them.
            "p + p { visibility: hidden }",
            "p.secret ~ p { visibility: hidden }",
            "p:last-child { visibility: hidden }",
            "p:first-of-type { visibility: hidden }",
            "p:nth-last-child(2) { visibility: hidden }",
            "p:nth-of-type(2) { visibility: hidden }",
            // A selector the walk cannot settle may hide.
            "p:has(b) { visibility: hidden }",
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
            "h1 + p { visibility: hidden }",
            "h1 ~ p { visibility: hidden }",
            "p + p.secret { visibility: hidden }",
            "p:only-of-type { visibility: hidden }",
            "p:nth-last-child(3) { visibility: hidden }",
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
    fn sibling_selectors_match_exactly() {
        let hidden = |rule: &str, body: &str| converts_hidden(&[rule], body);
        // `+` passes over text between elements; `~` reaches back past
        // other elements, and neither crosses to another parent.
        assert!(hidden(
            "h2 + p { visibility: hidden }",
            "<h2>T</h2> text <p>SECRET</p>"
        ));
        assert!(!hidden(
            "h2 + p { visibility: hidden }",
            "<h2>T</h2><div/><p>SECRET</p>"
        ));
        assert!(hidden(
            "h2 ~ p { visibility: hidden }",
            "<h2>T</h2><div/><p>SECRET</p>"
        ));
        assert!(!hidden(
            "h2 ~ p { visibility: hidden }",
            "<div><h2>T</h2></div><p>SECRET</p>"
        ));
        assert!(hidden(
            "div > h2 + p { visibility: hidden }",
            "<div><h2>T</h2><p>SECRET</p></div>"
        ));
        assert!(!hidden(
            "p:not(h2 + p) { visibility: hidden }",
            "<h2>T</h2><p>SECRET</p>"
        ));
        // Far into a long chapter the first siblings are kept, and those
        // let go between are known by name, id, and class.
        let many = "<p>x</p>".repeat(500);
        let long = |before: &str| format!(r#"{before}{many}<p class="s">SECRET</p>"#);
        assert!(hidden(
            "h1 ~ p.s { visibility: hidden }",
            &long("<h1>T</h1>")
        ));
        assert!(!hidden(
            "h2 ~ p.s { visibility: hidden }",
            &long("<h1>T</h1>")
        ));
        let middle = format!(r#"{many}<h2>T</h2>{many}<p class="s">SECRET</p>"#);
        assert!(hidden("h2 ~ p.s { visibility: hidden }", &middle));
        assert!(hidden("p + p.s { visibility: hidden }", &middle));
        assert!(!hidden("h2 + p.s { visibility: hidden }", &middle));
        // A sibling let go settles a `~` step, with the part of the
        // selector before it, as surely as one kept: a reader starts a new
        // line for the box, and words run together.
        let filler = |count: usize| "<i>x</i>".repeat(count);
        let gone = format!(
            r#"<div>{}<i class="key">k</i>{}<span>Balance due</span><span class="amt">Grand total</span></div>"#,
            filler(33),
            filler(110)
        );
        let breaks = |rule: &str| walk(&[rule], &gone).fuses_blocks;
        assert!(breaks(".key ~ .amt { display: block }"));
        assert!(breaks("i + .key ~ .amt { display: block }"));
        assert!(breaks("div > .key ~ .amt { display: block }"));
        assert!(breaks(".key ~ span + .amt { display: block }"));
        assert!(!breaks("b + .key ~ .amt { display: block }"));
        assert!(!breaks(".other ~ .amt { display: block }"));
        // A drop cap set by a sibling rule is settled.
        let fuses = |sheets: &[&str], body: &str| walk(sheets, body).fuses_blocks;
        let dropcap = r#"<h1>One</h1><p><span class="dc">W</span>hen the office opened</p>"#;
        assert!(!fuses(&["h1 + p span.dc { float: left }"], dropcap));
        assert!(fuses(
            &[".dc { display: block } h2 + p span.dc { float: left }"],
            dropcap
        ));
    }

    #[test]
    fn sibling_rules_for_absent_siblings_cost_a_lookup() {
        // A long chapter and many `~` rules for siblings it lacks.
        let sheet: String = (0..200)
            .map(|i| format!("h2.v{i} ~ p {{ float: none }}\n"))
            .collect();
        let (reader, anydoc) = cascade_for(&[&sheet]);
        let body = "<p>Line</p>".repeat(2000);
        let mut work = 0;
        chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("chapter walk");
        assert!(work <= 2 * 2000 * 200 + 10_000, "{work}");
    }

    #[test]
    fn sibling_rules_reaching_the_first_sibling_cost_a_lookup() {
        // A chapter heading's rules reaching every later paragraph, the
        // heading the first sibling, as a chapter opening might set them.
        let sheet = "h1.ct ~ p { display: block } h1.ct ~ p.t1::after { content: ''; display: block } h1 ~ p { float: none } h1 ~ p.t3 { position: static }";
        let (reader, anydoc) = cascade_for(&[sheet]);
        let body: String = std::iter::once(r#"<h1 class="ct">Chapter</h1>"#.to_string())
            .chain((0..3000).map(|line| format!(r#"<p class="t{}">Line {line}</p>"#, line % 4)))
            .collect();
        let mut work = 0;
        chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("chapter walk");
        assert!(work <= 3000 * 4 * 2 + 10_000, "{work}");
    }

    #[test]
    fn selector_backtracking_is_bounded() {
        // A child step on the left keeps every choice of the thirty
        // descendant steps open, which no chapter can afford to try.
        let rule = format!("[data-x] > {}p {{ visibility: hidden }}", "div ".repeat(30));
        let body = format!(
            "{}<p>SECRET</p>{}",
            "<div>".repeat(200),
            "</div>".repeat(200)
        );
        let (reader, anydoc) = cascade_for(&[&rule]);
        let started = std::time::Instant::now();
        let mut work = 0;
        let result = chapter_text(&chapter(&body), &reader, &anydoc, &mut work);
        assert!(matches!(result, Err(DocumentError::ResourceLimit)));
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
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
            &[".secret { visibility: hidden } p:has(b) { visibility: visible }"],
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
    fn text_an_svg_image_does_not_paint_converts_hidden() {
        let svg =
            r#"xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink""#;
        let html = r#"xmlns="http://www.w3.org/1999/xhtml""#;
        for body in [
            // Resources nothing refers to paint nothing.
            format!(r#"<svg {svg}><defs><text>Confidential 1,250.00</text></defs></svg>"#),
            format!(r#"<svg {svg}><symbol id="s"><text>SECRET</text></symbol></svg>"#),
            format!(r#"<svg {svg}><clipPath id="c"><text>SECRET</text></clipPath></svg>"#),
            format!(r#"<svg {svg}><mask id="m"><text>SECRET</text></mask></svg>"#),
            format!(r#"<svg {svg}><pattern id="p"><text>SECRET</text></pattern></svg>"#),
            format!(r#"<svg {svg}><marker id="k"><text>SECRET</text></marker></svg>"#),
            // A link in the page draws no symbol.
            format!(
                r##"<p><a href="#s">See</a></p><svg {svg}><symbol id="s"><text>SECRET</text></symbol></svg>"##
            ),
            // Gradients and filters paint no text, even where named.
            format!(
                r##"<svg {svg}><linearGradient id="g"><text>SECRET</text></linearGradient><rect fill="url(#g)"/></svg>"##
            ),
            // Elements SVG does not define, and text outside a text element.
            format!(r#"<svg {svg}><foo><text>SECRET</text></foo></svg>"#),
            format!(r#"<svg {svg}><g>SECRET</g></svg>"#),
            // A `switch` renders one child: the first whose conditions hold.
            format!(r#"<svg {svg}><switch><text>Shown</text> <text>SECRET</text></switch></svg>"#),
            format!(
                r#"<svg {svg}><switch><text requiredExtensions="http://ns.adobe.com/AdobeIllustrator/10.0/">SECRET</text></switch></svg>"#
            ),
            // HTML in a resource nothing refers to.
            format!(
                r#"<svg {svg}><defs><foreignObject><p {html}>SECRET</p></foreignObject></defs></svg>"#
            ),
        ] {
            assert!(converts_hidden(&[], &body), "{body}");
        }
        for body in [
            // Resources something refers to paint what they hold.
            format!(
                r##"<svg {svg}><symbol id="s"><text>Shown</text></symbol><use href="#s"/></svg>"##
            ),
            format!(
                r##"<svg {svg}><defs><text id="t">Shown</text></defs><use xlink:href="#t"/></svg>"##
            ),
            format!(
                r##"<svg {svg}><pattern id="p"><text>Shown</text></pattern><rect fill="url(#p)"/></svg>"##
            ),
            format!(
                r##"<svg {svg}><clipPath id="c"><text>Shown</text></clipPath><rect clip-path="url('#c')"/></svg>"##
            ),
            format!(
                r##"<svg {svg}><mask id="m"><text>Shown</text></mask><rect style="mask: url(#m)"/></svg>"##
            ),
            format!(
                r##"<svg {svg}><style>.r {{ marker-end: url(#k) }}</style><marker id="k"><text>Shown</text></marker><path class="r"/></svg>"##
            ),
            // Text in text elements, links, and paths, and HTML in an SVG image.
            format!(
                r##"<svg {svg}><a href="#x"><text>Shown <tspan>more</tspan><textPath href="#p">on a path</textPath></text></a></svg>"##
            ),
            format!(
                r#"<svg {svg}><foreignObject>Shown <p {html}>and more</p></foreignObject></svg>"#
            ),
            format!(
                r#"<svg {svg}><switch><foreignObject requiredExtensions="http://ns.adobe.com/AdobeIllustrator/10.0/"/><g><text>Shown</text></g></switch></svg>"#
            ),
            format!(
                r#"<svg {svg}><switch><foreignObject requiredExtensions="http://www.w3.org/1999/xhtml"><p {html}>Shown</p></foreignObject></switch></svg>"#
            ),
        ] {
            assert!(!converts_hidden(&[], &body), "{body}");
        }
        // A `::before` box in an SVG image's HTML shows, as in a page.
        let label = r#".x::before { content: "Balance due " }"#;
        assert!(drops_shown(
            &[label],
            &format!(
                r#"<svg {svg}><foreignObject><div {html} class="x">1,250.00</div></foreignObject></svg>"#
            )
        ));
        assert!(!drops_shown(
            &[r#"text::before { content: "Balance due " }"#],
            &format!(r#"<svg {svg}><text>1,250.00</text></svg>"#)
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
            // A packaged image converts as its alt text, which runs into
            // other text where digits meet.
            r#"<figure><img src="chart.png" alt="Figure 2"/><figcaption>2023 revenue</figcaption></figure>"#,
            r#"<div><img src="total.png" alt="Total 12"/></div><div>50.00</div>"#,
            // Digits meeting across a decimal point or separator, and a word
            // after punctuation.
            "<div>Rate 12.</div><div>5 percent</div>",
            "<div>Total 1,250</div><div>.00 due</div>",
            "<div>Paid in full.</div><div>Next year</div>",
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
            // Closing punctuation set on a line of its own joins the word
            // before it as written.
            "<div>Paid in full</div><div>. Next year</div>",
            "<div>See the note</div><div>), then continue</div>",
        ] {
            assert!(!fuses(body), "{body}");
        }
    }

    #[test]
    fn blocks_inside_links_run_into_the_text_around_them() {
        let fuses = |body: &str| walk(&[], body).fuses_blocks;
        // AnyDoc flattens a link's blocks into the run around the link:
        // the first and last with nothing between them and the text on
        // either side, a list, table, quote, or `pre` reduced to its text.
        for body in [
            r#"<div>Note:<a id="c1"><h2>Total 1,250.00</h2></a></div>"#,
            "<div>Balance due<a><p>1,250.00</p></a></div>",
            r##"<div>Balance due<a href="#nowhere"><p>1,250.00</p></a></div>"##,
            r#"<div>Balance due<a href="https://example.com/x"><p>1,250.00</p></a></div>"#,
            "<div><a><p>Balance due</p></a>1,250.00</div>",
            r#"<a id="c1"><h2>Chapter One</h2></a>Opening balance 1,250.00"#,
            "<p>Balance due<a><div><p>1,250.00</p></div></a></p>",
            "<div>Balance due<a><blockquote>1,250.00</blockquote></a></div>",
            "<div>Balance due<a><ul><li>1,250.00</li></ul></a></div>",
            "<div>Balance due<a><table><tr><td>1,250.00</td></tr></table></a></div>",
            "<div>Balance due<a><pre>1,250.00</pre></a></div>",
            // White space or a lone line break before a block inside the
            // link is dropped with the empty paragraph it would start, and
            // a block drops its own leading white space.
            "<div>Note:<a> <h2>Total</h2></a></div>",
            "<div>Note:<a><h2> Total</h2></a></div>",
            "<div>Note:<a><br/><p>Total</p></a></div>",
            "<div>Note:<a><p>Total</p> </a>Due</div>",
            // A rule converts as nothing, and so does an empty paragraph,
            // where a reader still starts a new line.
            "<div>Note:<a><hr/></a>Total</div>",
            "<div>Balance due<a><p> </p>1,250.00</a> </div>",
            // Only the last of several blocks runs into what follows.
            "<div><a><p>A</p><p>B</p></a>C</div>",
            // A link inside a link.
            "<div>Note:<a><a><p>Total</p></a></a></div>",
        ] {
            assert!(fuses(body), "{body}");
        }
        for body in [
            "<div>Balance due <a><p>1,250.00</p></a></div>",
            // AnyDoc joins a link's blocks with line breaks, and the items
            // of a list inside one with spaces.
            "<div><a><p>A</p><p>B</p></a></div>",
            "<div>A<a>B<p>C</p></a></div>",
            "<div><a><p>A</p>tail</a></div>",
            "<div>Items: <a><ul><li>A</li><li>B</li></ul></a> </div>",
            "<div>Note:<a><br/>Total</a></div>",
            "<div>Note: <a><p>X</p></a> Y</div>",
            // Only a link splices its blocks; other inline elements do not.
            "<div>Balance due<span><p>1,250.00</p></span></div>",
            "<div>Balance due<b><p>1,250.00</p></b></div>",
            r#"<section><a id="c1"><h2>Summary</h2></a><p>Total 1,250.00</p></section>"#,
            r#"<a id="c1"><p>Balance due</p></a><p>1,250.00</p>"#,
        ] {
            assert!(!fuses(body), "{body}");
        }
    }

    #[test]
    fn how_a_reader_lays_boxes_out_decides_their_lines() {
        let fuses = |sheets: &[&str], body: &str| walk(sheets, body).fuses_blocks;
        // A reader starts a new line for a box its style makes a block, and
        // for a block element AnyDoc does not know.
        assert!(fuses(
            &[".line { display: block }"],
            r#"<p><span class="line">Balance due</span><span class="line">1,250.00</span></p>"#
        ));
        assert!(fuses(
            &[],
            r#"<p><span style="display:block">Balance due</span><span style="display:block">1,250.00</span></p>"#
        ));
        assert!(fuses(
            &[],
            "<div>Balance due<address>1,250.00</address></div>"
        ));
        // A floated box with more than a drop cap's letters stands apart.
        assert!(fuses(
            &[".side { float: left }"],
            r#"<div class="side">Note</div><div>1,250.00</div>"#
        ));
        // A line break, rule, or empty block AnyDoc's styles skip, where a
        // reader shows it.
        assert!(fuses(
            &["br { display: none } p br { display: inline }"],
            "<p>Balance due<br/>1,250.00</p>"
        ));
        assert!(fuses(
            &["hr { display: none } div hr { display: block }"],
            "<div>Balance due<hr/>1,250.00</div>"
        ));
        assert!(fuses(
            &[".a.b { display: none }"],
            r#"<div>Balance due<p class="a.b"></p>1,250.00</div>"#
        ));
        // An inline box, a floated drop cap, and a line break both hide
        // keep the reader's line.
        assert!(!fuses(
            &[],
            r#"<div style="display:inline">Balance </div><div style="display:inline">due</div>"#
        ));
        assert!(!fuses(
            &[".x { display: inline-block }"],
            r#"<div class="x">A</div><div class="x">B</div>"#
        ));
        assert!(!fuses(
            &[".dropcap { float: left; font-size: 3em }"],
            r#"<div class="opening"><div class="dropcap">O</div><div class="rest">nce the office opened</div></div>"#
        ));
        assert!(!fuses(
            &[],
            r#"<p><span style="float:left">O</span>nce upon a time</p>"#
        ));
        assert!(!fuses(
            &["br { display: none }"],
            "<p>Balance due<br/>1,250.00</p>"
        ));
        // Sibling rules apply where the siblings are: the first box after
        // the paragraph is a block, the second is not.
        assert!(fuses(
            &[".x { display: inline } p + .x { display: block }"],
            r#"<p>A</p><div class="x">Balance due</div><div class="x">1,250.00</div>"#
        ));
        assert!(!fuses(
            &[".x { display: inline } h1 + .x { display: block }"],
            r#"<p>A</p><div class="x">Balance due</div><div class="x">1,250.00</div>"#
        ));
        // A rule the walk cannot settle counts where digits meet, which the
        // Markdown reads as one number.
        assert!(fuses(
            &[".x { display: inline } .x:has(b) { display: block }"],
            r#"<p>A</p><div class="x">Units 12</div><div class="x">50 shipped</div>"#
        ));
        assert!(!fuses(
            &[".x { display: inline } .x:has(b) { display: block }"],
            r#"<p>A</p><div class="x">Balance due</div><div class="x">1,250.00</div>"#
        ));
        // A block `::before` or `::after` box breaks the line around an
        // inline box, even an empty one.
        let glossary = "dt { display: inline } dd { display: inline; margin: 0 }";
        let entries = "<dl><dt>Basis</dt><dd>: what you paid.</dd><dt>Carryover</dt><dd>: an amount moved.</dd></dl>";
        assert!(!fuses(&[glossary], entries));
        assert!(fuses(
            &[glossary, "dd:after { content: ''; display: block }"],
            entries
        ));
        assert!(fuses(
            &[".br::after { content: ''; display: block }"],
            r#"<p><span class="br">Balance due</span>1,250.00</p>"#
        ));
        assert!(fuses(
            &[".brk::before { content: ''; display: block }"],
            r#"<p>Balance due<span class="brk"/>1,250.00</p>"#
        ));
        assert!(!fuses(
            &[".br::after { content: ''; display: inline }"],
            r#"<p><span class="br">Balance due</span>1,250.00</p>"#
        ));
        // A floated block box leaves the line, as a positioned one does.
        assert!(!fuses(
            &[".ic::before { content: ''; display: block; float: left }"],
            r#"<p>Total:<span class="ic">1,250.00</span> due.</p>"#
        ));
    }

    #[test]
    fn items_floats_links_and_svg_text_keep_their_own_lines() {
        let fuses = |sheets: &[&str], body: &str| walk(sheets, body).fuses_blocks;
        let math = r#"xmlns="http://www.w3.org/1998/Math/MathML""#;
        let svg = r#"xmlns="http://www.w3.org/2000/svg""#;
        for (sheets, body) in [
            // Each flex or grid item is a block, text straight inside too.
            (
                &[".s { display: flex; flex-direction: column }"][..],
                r#"<div class="s"><span>Balance due</span><span>1,250.00</span></div>"#.to_string(),
            ),
            (
                &[".s { display: grid }"],
                r#"<div class="s"><span>Item</span><span>1,250.00</span></div>"#.into(),
            ),
            (
                &[],
                r#"<div style="display:flex">Balance due<span>1,250.00</span></div>"#.into(),
            ),
            // A floated or positioned box holding digits is no drop cap,
            // unless a single figure floated to open its paragraph.
            (
                &[".f { float: left }"],
                r#"<p><span class="f">10</span>250 units received</p>"#.into(),
            ),
            (
                &[".f { float: left }"],
                r#"<p>Total <span class="f">1</span>914 units</p>"#.into(),
            ),
            (
                &[".ln { float: right }"],
                r#"<p><span class="ln">5</span>And then the river rose.</p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="position:absolute">1</span>914 units</p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="float:right">12</span>Widgets</p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="position:absolute;left:300px">12</span>50 units</p>"#.into(),
            ),
            // AnyDoc drops a link holding only a line break, and flattens
            // a display formula in a link into the text around it.
            (&[], "<p>Balance due<a><br/></a>1,250.00</p>".into()),
            (
                &[],
                "<p>Balance due<a><div><span>1,250.00</span><h3>Paid</h3></div></a></p>".into(),
            ),
            (
                &[],
                format!(r#"<p><a><math {math} display="block"><mn>12</mn></math></a>50 units</p>"#),
            ),
            (
                &[],
                format!(
                    r#"<p>Total<a> <math {math} display="block">
                    <mn>12</mn> </math> </a>50 units</p>"#
                ),
            ),
            // A block image, and each placed text of an SVG image.
            (
                &[],
                r#"<p>Balance due<img src="rule.png" alt="" style="display:block"/>1,250.00</p>"#
                    .into(),
            ),
            (
                &[],
                format!(
                    r#"<svg {svg}><text x="10" y="20">Balance due</text><text x="10" y="45">1,250.00</text></svg>"#
                ),
            ),
            (
                &[],
                format!(
                    r#"<svg {svg}><text><tspan x="10" dy="1.2em">Balance due</tspan><tspan x="10" dy="1.2em">1,250.00</tspan></text></svg>"#
                ),
            ),
            // A line feed a `::before` box keeps.
            (
                &[r#".amt::before { content: "\A"; white-space: pre }"#],
                r#"<p>Balance due<span class="amt">1,250.00</span></p>"#.into(),
            ),
            // Cells of a table laid out by style.
            (
                &[".row { display: table } .cell { display: table-cell }"],
                r#"<p class="row"><span class="cell">Intake</span><span class="cell">7</span></p>"#
                    .into(),
            ),
        ] {
            assert!(fuses(sheets, &body), "{body}");
        }
        for (sheets, body) in [
            // AnyDoc keeps white space between items, a line break beside
            // other text in a link, and an inline formula's delimiters.
            (
                &[".s { display: flex }"][..],
                r#"<div class="s"><span>Balance due</span> <span>1,250.00</span></div>"#
                    .to_string(),
            ),
            (&[], "<p>Balance due<span><br/></span>1,250.00</p>".into()),
            (&[], "<p>x<a>Balance due<br/></a>1,250.00</p>".into()),
            (
                &[],
                format!(r#"<p>due<a><math {math}><mn>12</mn></math></a>50 units</p>"#),
            ),
            // A drop cap of a word's first letters, going on in lower case,
            // and a year's first figure opening its paragraph.
            (
                &[],
                r#"<p><span style="float:left">Onc</span>e the office opened</p>"#.into(),
            ),
            (
                &[".dc { float: left; font-size: 3em }"],
                r#"<h1>Four</h1><p><span class="dc">1</span>914 began quietly.</p>"#.into(),
            ),
            // A box a reader keeps in the line, though AnyDoc splits a link's
            // content at the heading inside it.
            (
                &[".il { display: inline }"],
                r#"<p>Balance due<a><div class="il"><span>1,250.00</span><h3>Paid</h3></div></a></p>"#
                    .into(),
            ),
            // A line feed collapsed to a space, or in a box that is not there.
            (
                &[r#".amt::before { content: "\A" }"#],
                r#"<p>Balance due<span class="amt">1,250.00</span></p>"#.into(),
            ),
            (
                &[r#".amt::before { content: "\A"; white-space: pre; display: none }"#],
                r#"<p>Balance due<span class="amt">1,250.00</span></p>"#.into(),
            ),
            // A shift that may be a superscript's, where no digits meet.
            (
                &[],
                format!(r#"<svg {svg}><text>Area<tspan dy="-4">2</tspan></text></svg>"#),
            ),
        ] {
            assert!(!fuses(sheets, &body), "{body}");
        }
        // The one uncertain break that counts: digits meeting.
        assert!(fuses(
            &[r#".amt::before { content: "\A" }"#],
            r#"<p>Units 12<span class="amt">50</span></p>"#
        ));
    }

    #[test]
    fn text_generated_content_adds_is_shown_and_not_converted() {
        for (sheets, body) in [
            (
                r#".n::before { content: "\2212" }"#,
                r#"<p>Net change <span class="n">1,250.00</span></p>"#,
            ),
            (
                r#".n::after { content: " (restated)" }"#,
                r#"<p><span class="n">1,250.00</span></p>"#,
            ),
            (
                r#".c::before { content: counter(item) ". " }"#,
                r#"<p class="c">First</p>"#,
            ),
            (
                r#".e::before { content: "Total" }"#,
                r#"<p>Due <span class="e"/></p>"#,
            ),
            (
                r#".tip::after { content: attr(title); display: block; position: absolute }"#,
                r#"<p>See <abbr class="tip" title="adjusted gross income">AGI</abbr>.</p>"#,
            ),
            // A box whose visibility is not settled, and one visible on an
            // element that is not.
            (
                r#".tip::after { content: attr(title); visibility: var(--tip) }"#,
                r#"<p>See <abbr class="tip" title="adjusted gross income">AGI</abbr>.</p>"#,
            ),
            (
                r#".tip::after { content: attr(title) } .tip:has(b)::after { opacity: 0 }"#,
                r#"<p>See <abbr class="tip" title="adjusted gross income">AGI</abbr>.</p>"#,
            ),
            (
                r#".x { visibility: hidden } .x::before { content: "Label"; visibility: visible }"#,
                r#"<p>Due <span class="x"/></p>"#,
            ),
            // Signs beside the digits of an amount.
            (
                r#".neg::before { content: "(" } .neg::after { content: ")" }"#,
                r#"<p>Loss <span class="neg"><b>1,250.00</b></span></p>"#,
            ),
            (
                r#".pct::after { content: "%" }"#,
                r#"<p>Rate <span class="pct">5</span></p>"#,
            ),
            (
                r#".n::before { content: "-" }"#,
                r#"<p><span class="n"/> 1,250.00</p>"#,
            ),
        ] {
            assert!(drops_shown(&[sheets], body), "{sheets} {body}");
        }
        for (sheets, body) in [
            // Ornaments: quote marks, dashes, bullets, and line feeds.
            (r#"q::before { content: "\201C" }"#, "<p><q>Quoted</q></p>"),
            (
                r#".d::before { content: "\2014 " }"#,
                r#"<p class="d">Aside</p>"#,
            ),
            (
                r#".b::before { content: "\2022" }"#,
                r#"<p class="b">Point</p>"#,
            ),
            // A box `display: none` takes away, or on an element hidden.
            (
                r#".n::before { content: "\2212"; display: none }"#,
                r#"<p><span class="n">1,250.00</span></p>"#,
            ),
            // A box kept unseen until the pointer rests on its element.
            (
                r#".gl::after { content: attr(title); position: absolute; opacity: 0 } .gl:hover::after { opacity: 1 }"#,
                r#"<p>File the <span class="gl" title="adjusted gross income">AGI</span> worksheet.</p>"#,
            ),
            (
                r#".gl::after { content: attr(title); visibility: hidden } .gl:hover::after { visibility: visible }"#,
                r#"<p>File the <span class="gl" title="adjusted gross income">AGI</span> worksheet.</p>"#,
            ),
            (
                r#".n { visibility: hidden } .n::before { content: "\2212" }"#,
                r#"<p><span class="n"/>1,250.00</p>"#,
            ),
            (
                r#".n { display: none } .n::before { content: "\2212" }"#,
                r#"<p><span class="n"/></p>"#,
            ),
            // Content a rule may not give.
            (
                r#".n:has(b)::before { content: "\2212" }"#,
                r#"<p>A</p><p class="n">1</p>"#,
            ),
            // A sign beside words: a bullet, or parentheses around a note.
            (
                r#"li.dash::before { content: "- " }"#,
                r#"<ul><li class="dash">Item</li></ul>"#,
            ),
            (
                r#".note::before { content: "(" } .note::after { content: ")" }"#,
                r#"<p>Total <span class="note">see page</span> due</p>"#,
            ),
        ] {
            assert!(!drops_shown(&[sheets], body), "{sheets}");
        }
    }

    #[test]
    fn generated_signs_are_lost_beside_the_digits_they_meet() {
        for (sheets, body) in [
            // A decimal point or thousands separator before digits, from
            // the box before them or the box after the digits before it.
            (
                r#".c::before { content: "." }"#,
                r#"<p>Price 1<span class="c">99</span></p>"#,
            ),
            (
                r#".d::after { content: "." }"#,
                r#"<p>Price <span class="d">1</span>99</p>"#,
            ),
            (
                r#".b::before { content: "," }"#,
                r#"<p>Total <span class="a">1</span><span class="b">250</span> units</p>"#,
            ),
            // A sign on an empty element meets the digits on either side.
            (
                r#".s::after { content: "$" }"#,
                r#"<p>Total <span class="s"/>1,250.00</p>"#,
            ),
            (
                r#".p::before { content: "%" }"#,
                r#"<p>Rate 12<span class="p"/> of total</p>"#,
            ),
            (
                r#".s::after { content: "$" }"#,
                r#"<p>Total <span class="s">due</span>1,250.00</p>"#,
            ),
            // Currency signs and dashes past the ASCII ones.
            (
                r#".c::before { content: "\20B9" }"#,
                r#"<p>Fee <span class="c">1,250</span> due</p>"#,
            ),
            (
                r#".r::before { content: "\2013" }"#,
                r#"<p>Years 1914<span class="r">1918</span></p>"#,
            ),
            // Separators between spans, and between list items inside a
            // link, which AnyDoc joins with a space and no marker.
            (
                r#".years span:not(:last-child)::after { content: " - " }"#,
                r#"<p class="years"><span>1914</span><span>1918</span></p>"#,
            ),
            (
                r#"li + li::before { content: "- " }"#,
                "<div>Years <a><ul><li>1914</li><li>1918</li></ul></a></div>",
            ),
        ] {
            assert!(drops_shown(&[sheets], body), "{sheets} {body}");
        }
        for (sheets, body) in [
            // A period closing a number, or a separator apart from the
            // digits after it.
            (
                r#".n::after { content: "." }"#,
                r#"<p><span class="n">1</span> Keep copies.</p>"#,
            ),
            (
                r#".c::before { content: "." }"#,
                r#"<p>Price 1<span class="c"> each</span></p>"#,
            ),
            (
                r#".s::after { content: "." }"#,
                r#"<p>Total <span class="s"/> 250</p>"#,
            ),
            // A sign after a block, which starts a line of its own.
            (
                r#".s::after { content: "$" } .s { display: block }"#,
                r#"<div><span class="s">Total</span> 1,250.00</div>"#,
            ),
            // A list item's sign: AnyDoc's list marker stands in for it.
            (
                r#"li::before { content: "- " }"#,
                "<ul><li>2 cups flour</li></ul>",
            ),
            (
                r#"li { display: inline } li + li::before { content: " - " }"#,
                "<ul><li>2024</li><li>2025</li></ul>",
            ),
            (
                r#"li { display: inline } li:not(:last-child)::after { content: " - " }"#,
                "<ul><li>1914</li><li>1918</li></ul>",
            ),
        ] {
            assert!(!drops_shown(&[sheets], body), "{sheets} {body}");
        }
    }

    #[test]
    fn rules_for_ancestors_a_chapter_lacks_cost_a_lookup() {
        // Rules for sections the chapter does not have, as a large book's
        // stylesheet carries: each is set aside without walking up.
        let sheet: String = (0..200)
            .map(|i| format!("body div.v{i} div p span {{ float: none }}\n"))
            .collect();
        let (reader, anydoc) = cascade_for(&[&sheet]);
        let body = "<p><span>Line</span> text</p>".repeat(50);
        let mut work = 0;
        chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("chapter walk");
        assert!(work <= 50 * 200, "{work}");
        // A rule whose ancestors are there is still matched in full.
        let (reader, anydoc) = cascade_for(&[".v1 p span { display: block }"]);
        let mut work = 0;
        let found = chapter_text(
            &chapter(
                r#"<div class="V1"><p><span>Balance due</span><span>1,250.00</span></p></div>"#,
            ),
            &reader,
            &anydoc,
            &mut work,
        )
        .expect("chapter walk");
        assert!(!found.fuses_blocks, "a class in another case may not match");
        let found = chapter_text(
            &chapter(
                r#"<div class="v1"><p><span>Balance due</span><span>1,250.00</span></p></div>"#,
            ),
            &reader,
            &anydoc,
            &mut work,
        )
        .expect("chapter walk");
        assert!(found.fuses_blocks);
    }

    #[test]
    fn alt_text_runs_into_other_text_only_where_digits_meet() {
        let fuses = |body: &str| walk(&[], body).fuses_blocks;
        // A reader shows the image, not its alt text: pandoc 2 gives an
        // implicit figure's image its caption as alt text, and other books
        // give it a word of its own.
        assert!(!fuses(
            r#"<figure><img src="chart.png" alt="Revenue by quarter"/><figcaption>Revenue by quarter</figcaption></figure>"#
        ));
        assert!(!fuses(
            r#"<figure><img src="chart.png" alt="Revenue by quarter"/><figcaption>Revenue <em>by quarter</em></figcaption></figure>"#
        ));
        assert!(!fuses(
            r#"<figure><img src="chart.png" alt="Chart"/><figcaption>Revenue by quarter</figcaption></figure>"#
        ));
        // Digits on both sides read as one number.
        assert!(fuses(
            r#"<figure><img src="chart.png" alt="2023"/><figcaption>2023</figcaption></figure>"#
        ));
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
                Position::default(),
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

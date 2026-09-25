//! Stylesheet evaluation for the EPUB hidden-content check.
//!
//! A chapter's text is flagged when a reading system would hide it and
//! AnyDoc would still convert it. Two models answer those questions.
//!
//! - The reader model follows CSS as reading systems apply it: the CSS
//!   Syntax 3 tokenizer (comments, escapes, strings), media queries and
//!   `@supports`, `@scope`, and `@container` conditions as Chromium
//!   evaluates them on the screens of the readers the check follows (see
//!   [`VIEWPORT_SIZES`]), the selector grammar, the cascade with its
//!   layers, and the user-agent rules that hide content. Sibling
//!   combinators and positions among siblings are matched exactly:
//!   positions from a pass over the chapter before the walk, `+` from the
//!   earlier siblings the walk keeps, and a rule's `~` step from the first
//!   sibling that fits it, noted as each sibling ends, which costs a lookup
//!   however far back that sibling is. Where it cannot decide (`:has()`,
//!   an unknown pseudo-class, a value set through `var()`, a condition on
//!   a container's size or on a screen only some readers have), it lets a
//!   hiding rule apply and keeps a showing rule from overriding one, so it
//!   errs toward finding hidden text.
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
//! that are blocks or keep a line feed. Flex items in a row may touch; in
//! a column, with a gap, spread along the line, or with a margin or padding
//! between them they stand apart, as grid items do. The chapter walk
//! mirrors AnyDoc's inline runs, including the way it flattens a link's
//! blocks into the text around it. Text runs together where a word, or a
//! number, meets another with nothing between; closing punctuation joins
//! the word before it as written. A break only a rule the walk cannot
//! settle gives counts where digits meet, which the Markdown reads as one
//! number.
//!
//! Text a `::before` or `::after` box shows is text AnyDoc drops: flagged
//! when it holds letters or digits, for a sign an amount reads by ("−",
//! "(", "%", "$") when it meets digits, and for anything else it shows, a
//! space, a comma, or a colon, between digits AnyDoc's text then runs
//! together. A list item's bullet, which AnyDoc's list marker stands in
//! for, is not.

use std::collections::HashMap;
use std::rc::Rc;

use super::DocumentError;

/// Selectors in one rule's list, compound selectors in one complex
/// selector, and nested selector lists (`:not(:is(…))`) evaluated before a
/// selector counts as undecidable.
const MAX_SELECTORS_PER_RULE: usize = 256;
const MAX_COMPOUNDS: usize = 32;
const MAX_SELECTOR_NESTING: usize = 8;
/// Compound selectors one selector may hold, those of the lists in its
/// pseudo-classes and of the rules its `&`s stand for counted, before it
/// counts as undecidable: rules nested in each other share their parents'
/// selectors, which `&`s taken twice at every level would multiply past
/// what a match can walk.
const MAX_SELECTOR_WEIGHT: u32 = 4096;
/// `@import` statements one stylesheet may carry.
pub(super) const MAX_IMPORTS_PER_SHEET: usize = 256;
/// Style rules that set `display`, `visibility`, `content-visibility`,
/// `float`, or `position`, how flex items stand, or a margin or padding, or
/// style `::before` and `::after` boxes, across a package's stylesheets.
/// Real books carry far fewer.
pub(super) const MAX_STYLE_RULES: usize = 16_384;
/// Compound-selector evaluations across a package: one element tested
/// against one rule costs one per compound it reaches, and one when the
/// ancestor filter sets the rule aside. Reading an inline style costs one
/// for each token, and looking a custom property up in it one for each
/// declaration.
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

/// The component values of a token run, white space between them left out:
/// a whole block or function, or one token each.
fn components(tokens: &[Token]) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let end = skip_component(tokens, index);
        if tokens[index] != Token::Whitespace {
            parts.push(&tokens[index..end]);
        }
        index = end;
    }
    parts
}

/// Whether a component value is the keyword `wanted`, in any case.
fn is_word(part: &[Token], wanted: &str) -> bool {
    matches!(part, [Token::Ident(word)] if word.eq_ignore_ascii_case(wanted))
}

/// The contents of a parenthesized block or a function's arguments.
fn block_contents(part: &[Token]) -> &[Token] {
    block_at(part, 0).0
}

// ---------------------------------------------------------------------------
// The readers the check follows

/// How surely something holds across the readers the check follows (see
/// [`VIEWPORT_SIZES`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Applies {
    /// On none of them.
    No,
    /// The check cannot tell.
    Doubt,
    /// On some and not on others, as their screens decide.
    Varies,
    /// On all of them.
    Yes,
}

impl Applies {
    /// How a selector's match applies: one the check cannot settle is in
    /// doubt.
    fn matched(certainty: Tri) -> Self {
        match certainty {
            Tri::Yes => Applies::Yes,
            Tri::Maybe => Applies::Doubt,
            Tri::No => Applies::No,
        }
    }

    /// Whether it applies on every reader: `Maybe` where that varies or is
    /// in doubt.
    fn everywhere(self) -> Tri {
        match self {
            Applies::Yes => Tri::Yes,
            Applies::No => Tri::No,
            Applies::Doubt | Applies::Varies => Tri::Maybe,
        }
    }

    /// Where what it guards fails to apply.
    fn not(self) -> Self {
        match self {
            Applies::Yes => Applies::No,
            Applies::No => Applies::Yes,
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Media queries

/// The readers the check follows, as Chromium reports their screens:
/// viewports 320 to 1280 CSS pixels wide and as tall, so either way up, at
/// 1 to 3 device pixels to the CSS pixel, in colour of 8 bits a channel;
/// with or without a mouse, a touch screen, scripting, a dark scheme,
/// reduced motion, or forced colours.
const VIEWPORT_SIZES: (f64, f64) = (320.0, 1280.0);
const PIXEL_RATIOS: (f64, f64) = (1.0, 3.0);
const COLOR_BITS: f64 = 8.0;
/// Viewport sizes one media query list is tried at, and parentheses it may
/// nest, before it counts as one the check cannot settle.
const MAX_MEDIA_SAMPLES: usize = 4096;
const MAX_MEDIA_NESTING: usize = 16;

/// A media query as Media Queries 4 reads it: its media type, which holds
/// on a screen or not, and the features it tests.
enum MediaTest {
    Not(Box<MediaTest>),
    And(Vec<MediaTest>),
    Or(Vec<MediaTest>),
    /// The viewport's width, height, or width over height against a value,
    /// in CSS pixels or as a ratio.
    Size(Axis, Comparison, f64),
    /// Whether the viewport is at least as tall as it is wide.
    Portrait(bool),
    /// A test whose outcome does not follow the viewport's size.
    Fixed(Outcome),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Width,
    Height,
    Ratio,
}

#[derive(Clone, Copy)]
enum Comparison {
    Less,
    AtMost,
    Equal,
    AtLeast,
    Greater,
}

impl Comparison {
    fn holds(self, value: f64, bound: f64) -> bool {
        match self {
            Comparison::Less => value < bound,
            Comparison::AtMost => value <= bound,
            Comparison::Equal => value == bound,
            Comparison::AtLeast => value >= bound,
            Comparison::Greater => value > bound,
        }
    }

    /// The comparison read from the value's side (`600px < width` as
    /// `width > 600px`).
    fn flipped(self) -> Self {
        match self {
            Comparison::Less => Comparison::Greater,
            Comparison::AtMost => Comparison::AtLeast,
            Comparison::Equal => Comparison::Equal,
            Comparison::AtLeast => Comparison::AtMost,
            Comparison::Greater => Comparison::Less,
        }
    }
}

/// What a media test comes to at one viewport size: which of holding,
/// failing, and unknown (a test Chromium does not know, which fails) it may
/// come to, and whether more than one because the check cannot tell rather
/// than because readers of that size differ.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Outcome {
    may: u8,
    doubt: bool,
}

const HOLDS: u8 = 1;
const FAILS: u8 = 2;
const UNKNOWN: u8 = 4;

impl Outcome {
    const HOLDS: Outcome = Outcome {
        may: HOLDS,
        doubt: false,
    };
    const FAILS: Outcome = Outcome {
        may: FAILS,
        doubt: false,
    };
    const UNKNOWN: Outcome = Outcome {
        may: UNKNOWN,
        doubt: false,
    };
    /// Holds for some readers and fails for others.
    const VARIES: Outcome = Outcome {
        may: HOLDS | FAILS,
        doubt: false,
    };
    const DOUBT: Outcome = Outcome {
        may: HOLDS | FAILS,
        doubt: true,
    };

    fn of(holds: bool) -> Self {
        if holds {
            Outcome::HOLDS
        } else {
            Outcome::FAILS
        }
    }

    fn not(self) -> Self {
        let swapped = [(HOLDS, FAILS), (FAILS, HOLDS), (UNKNOWN, UNKNOWN)]
            .iter()
            .filter(|(from, _)| self.may & from != 0)
            .fold(0, |may, (_, to)| may | to);
        Outcome {
            may: swapped,
            doubt: self.doubt,
        }
    }

    /// Both of two tests, or either, three-valued: `and` fails where either
    /// fails, and is unknown where neither fails and one is unknown; `or`
    /// the other way about.
    fn join(self, other: Outcome, and: bool) -> Self {
        let (decides, keeps) = if and { (FAILS, HOLDS) } else { (HOLDS, FAILS) };
        let mut may = 0;
        for first in [HOLDS, FAILS, UNKNOWN] {
            for second in [HOLDS, FAILS, UNKNOWN] {
                if self.may & first == 0 || other.may & second == 0 {
                    continue;
                }
                may |= if first == decides || second == decides {
                    decides
                } else if first == UNKNOWN || second == UNKNOWN {
                    UNKNOWN
                } else {
                    keeps
                };
            }
        }
        Outcome {
            may,
            doubt: self.doubt || other.doubt,
        }
    }
}

/// Whether a media query list holds on the readers the check follows (see
/// [`VIEWPORT_SIZES`]): `Yes` where it holds on all of them, as `(min-width:
/// 0)` does, `No` on none, as `print` and `(max-width: 1px)`, and `Varies`
/// on some. An empty list holds, and a list where one of its queries does.
/// A query Chromium rejects, such as `screen screen`, `not only print`, or
/// an empty one, holds nowhere, and a feature it does not know fails. Each
/// query is tried at the viewport sizes either side of the widths, heights,
/// and ratios it names.
fn media_condition(tokens: &[Token]) -> Applies {
    let tokens = trim_whitespace(tokens);
    if tokens.is_empty() {
        return Applies::Yes;
    }
    let queries = split_top_level(tokens, &Token::Comma)
        .into_iter()
        .map(|query| {
            media_query(trim_whitespace(query)).unwrap_or(MediaTest::Fixed(Outcome::FAILS))
        })
        .collect();
    media_applies(&MediaTest::Or(queries))
}

/// How a `media` attribute or pseudo-attribute applies on the readers the
/// check follows; a media list too long to read may apply.
pub(super) fn media_attribute(value: &str) -> Applies {
    match tokenize(value) {
        Ok(tokens) => media_condition(&tokens),
        Err(_) => Applies::Doubt,
    }
}

/// How a media test applies, from what it comes to at the viewport sizes
/// either side of each bound it names.
fn media_applies(test: &MediaTest) -> Applies {
    let (low, high) = VIEWPORT_SIZES;
    let mut widths = vec![low, high];
    let mut heights = vec![low, high];
    media_bounds(test, &mut widths, &mut heights);
    for sizes in [&mut widths, &mut heights] {
        sizes.retain(|size| (low..=high).contains(size));
        sizes.sort_by(f64::total_cmp);
        sizes.dedup();
    }
    if widths.len() * heights.len() > MAX_MEDIA_SAMPLES {
        return Applies::Doubt;
    }
    let (mut holds, mut fails, mut doubt) = (false, false, false);
    for &width in &widths {
        for &height in &heights {
            let outcome = media_outcome(test, width, height);
            let may_hold = outcome.may & HOLDS != 0;
            let may_fail = outcome.may & (FAILS | UNKNOWN) != 0;
            doubt |= outcome.doubt && may_hold && may_fail;
            holds |= may_hold;
            fails |= may_fail;
        }
    }
    match (doubt, holds, fails) {
        (true, _, _) => Applies::Doubt,
        (false, true, false) => Applies::Yes,
        (false, false, _) => Applies::No,
        (false, true, true) => Applies::Varies,
    }
}

/// Add the viewport sizes at and just either side of each bound a test
/// names, and for a ratio those that give it at the smallest and largest.
fn media_bounds(test: &MediaTest, widths: &mut Vec<f64>, heights: &mut Vec<f64>) {
    let near = |bound: f64| [bound - 0.01, bound, bound + 0.01];
    match test {
        MediaTest::Not(inner) => media_bounds(inner, widths, heights),
        MediaTest::And(tests) | MediaTest::Or(tests) => {
            for test in tests {
                media_bounds(test, widths, heights);
            }
        }
        MediaTest::Size(Axis::Width, _, bound) => widths.extend(near(*bound)),
        MediaTest::Size(Axis::Height, _, bound) => heights.extend(near(*bound)),
        MediaTest::Size(Axis::Ratio, _, ratio) if *ratio > 0.0 => {
            let (low, high) = VIEWPORT_SIZES;
            widths.extend([low * ratio, high * ratio]);
            heights.extend([low / ratio, high / ratio]);
        }
        MediaTest::Size(..) | MediaTest::Portrait(_) | MediaTest::Fixed(_) => {}
    }
}

/// What a media test comes to at one viewport size.
fn media_outcome(test: &MediaTest, width: f64, height: f64) -> Outcome {
    match test {
        MediaTest::Not(inner) => media_outcome(inner, width, height).not(),
        MediaTest::And(tests) | MediaTest::Or(tests) => {
            let and = matches!(test, MediaTest::And(_));
            tests
                .iter()
                .map(|test| media_outcome(test, width, height))
                .reduce(|first, second| first.join(second, and))
                .unwrap_or(Outcome::of(and))
        }
        MediaTest::Size(axis, comparison, bound) => {
            let value = match axis {
                Axis::Width => width,
                Axis::Height => height,
                Axis::Ratio => width / height,
            };
            Outcome::of(comparison.holds(value, *bound))
        }
        MediaTest::Portrait(portrait) => Outcome::of((height >= width) == *portrait),
        MediaTest::Fixed(outcome) => *outcome,
    }
}

/// One media query: a media condition, or a media type, perhaps after
/// `not` or `only`, with a condition joined by `and`; `None` where
/// Chromium rejects it.
fn media_query(tokens: &[Token]) -> Option<MediaTest> {
    let parts = components(tokens);
    let word = |at: usize| match parts.get(at) {
        Some([Token::Ident(word)]) => Some(word.to_ascii_lowercase()),
        _ => None,
    };
    let (negated, kind_at) = match word(0).as_deref() {
        Some("not") if word(1).is_some() => (true, 1),
        Some("not") | None => return media_condition_parts(&parts, true, 0),
        Some("only") => (false, 1),
        Some(_) => (false, 0),
    };
    let kind = word(kind_at)?;
    if matches!(kind.as_str(), "only" | "not" | "and" | "or" | "layer") {
        return None;
    }
    // Print, speech, the deprecated types, and types Chromium does not
    // know, such as `amzn-mobi`, never hold on a screen.
    let screen = MediaTest::Fixed(Outcome::of(matches!(kind.as_str(), "all" | "screen")));
    let test = match &parts[kind_at + 1..] {
        [] => screen,
        [and, condition @ ..] if is_word(and, "and") => {
            MediaTest::And(vec![screen, media_condition_parts(condition, false, 0)?])
        }
        _ => return None,
    };
    Some(if negated {
        MediaTest::Not(Box::new(test))
    } else {
        test
    })
}

/// A media condition: `not` before one test in parentheses, or tests joined
/// all by `and` or all by `or`, which a media type's condition may not use.
fn media_condition_parts(
    parts: &[&[Token]],
    or_allowed: bool,
    nesting: usize,
) -> Option<MediaTest> {
    match parts {
        [not, operand] if is_word(not, "not") => {
            Some(MediaTest::Not(Box::new(media_in_parens(operand, nesting)?)))
        }
        [first, rest @ ..] if rest.len().is_multiple_of(2) => {
            let mut tests = vec![media_in_parens(first, nesting)?];
            let mut and = None;
            for pair in rest.chunks(2) {
                let joins_and = if is_word(pair[0], "and") {
                    true
                } else if is_word(pair[0], "or") && or_allowed {
                    false
                } else {
                    return None;
                };
                if and.is_some_and(|and| and != joins_and) {
                    return None;
                }
                and = Some(joins_and);
                tests.push(media_in_parens(pair[1], nesting)?);
            }
            Some(match and {
                None => tests.pop()?,
                Some(true) => MediaTest::And(tests),
                Some(false) => MediaTest::Or(tests),
            })
        }
        _ => None,
    }
}

/// A test in parentheses, a condition or a feature; anything else in
/// parentheses, or a function, is unknown to Chromium and fails.
fn media_in_parens(part: &[Token], nesting: usize) -> Option<MediaTest> {
    match part.first()? {
        Token::OpenParen => {
            if nesting >= MAX_MEDIA_NESTING {
                return Some(MediaTest::Fixed(Outcome::DOUBT));
            }
            let inner = trim_whitespace(block_contents(part));
            let parts = components(inner);
            Some(
                media_condition_parts(&parts, true, nesting + 1)
                    .or_else(|| media_feature(inner))
                    .unwrap_or(MediaTest::Fixed(Outcome::UNKNOWN)),
            )
        }
        Token::Function(_) => Some(MediaTest::Fixed(Outcome::UNKNOWN)),
        _ => None,
    }
}

/// A media feature in parentheses: its name alone, its name and a value
/// (`min-width: 600px`), or a range (`400px <= width < 900px`).
fn media_feature(tokens: &[Token]) -> Option<MediaTest> {
    let mut operands: Vec<&[Token]> = Vec::new();
    let mut comparisons = Vec::new();
    let (mut start, mut index) = (0, 0);
    while index < tokens.len() {
        let equals = tokens.get(index + 1) == Some(&Token::Delim('='));
        let comparison = match &tokens[index] {
            Token::Delim('<') if equals => Comparison::AtMost,
            Token::Delim('>') if equals => Comparison::AtLeast,
            Token::Delim('<') => Comparison::Less,
            Token::Delim('>') => Comparison::Greater,
            Token::Delim('=') => Comparison::Equal,
            _ => {
                index = skip_component(tokens, index);
                continue;
            }
        };
        operands.push(trim_whitespace(&tokens[start..index]));
        comparisons.push(comparison);
        index += if matches!(comparison, Comparison::AtMost | Comparison::AtLeast) {
            2
        } else {
            1
        };
        start = index;
    }
    operands.push(trim_whitespace(&tokens[start..]));
    let name = |operand: &[Token]| match operand {
        [Token::Ident(name)] => Some(name.to_ascii_lowercase()),
        _ => None,
    };
    match (operands.as_slice(), comparisons.as_slice()) {
        ([feature], []) => {
            let [Token::Ident(name), rest @ ..] = feature else {
                return None;
            };
            let name = name.to_ascii_lowercase();
            match trim_whitespace(rest) {
                [] => Some(media_feature_test(&name, FeatureQuery::Boolean)),
                [Token::Colon, value @ ..] => {
                    let value = trim_whitespace(value);
                    let ranged = [("min-", Comparison::AtLeast), ("max-", Comparison::AtMost)]
                        .into_iter()
                        .find_map(|(prefix, comparison)| {
                            let base = match name.strip_prefix("-webkit-") {
                                Some(rest) => format!("-webkit-{}", rest.strip_prefix(prefix)?),
                                None => name.strip_prefix(prefix)?.to_string(),
                            };
                            Some((base, comparison))
                        });
                    Some(match ranged {
                        Some((base, comparison)) => {
                            media_feature_test(&base, FeatureQuery::Range(comparison, value))
                        }
                        None => media_feature_test(&name, FeatureQuery::Plain(value)),
                    })
                }
                _ => None,
            }
        }
        ([first, second], [comparison]) => match (name(first), name(second)) {
            (Some(name), _) => Some(media_feature_test(
                &name,
                FeatureQuery::Range(*comparison, second),
            )),
            (None, Some(name)) => Some(media_feature_test(
                &name,
                FeatureQuery::Range(comparison.flipped(), first),
            )),
            (None, None) => None,
        },
        ([low, middle, high], [first, second]) => {
            let name = name(middle)?;
            let rising = |comparison: &Comparison| {
                matches!(comparison, Comparison::Less | Comparison::AtMost)
            };
            let falling = |comparison: &Comparison| {
                matches!(comparison, Comparison::Greater | Comparison::AtLeast)
            };
            if !(rising(first) && rising(second) || falling(first) && falling(second)) {
                return None;
            }
            Some(MediaTest::And(vec![
                media_feature_test(&name, FeatureQuery::Range(first.flipped(), low)),
                media_feature_test(&name, FeatureQuery::Range(*second, high)),
            ]))
        }
        _ => None,
    }
}

/// How a media feature is tested: by its name alone, against a value, or
/// in a range (`min-`, `max-`, or a comparison).
#[derive(Clone, Copy)]
enum FeatureQuery<'a> {
    Boolean,
    Plain(&'a [Token]),
    Range(Comparison, &'a [Token]),
}

/// How a media feature tests the readers the check follows (see
/// [`VIEWPORT_SIZES`]). A feature Chromium does not know, a value it
/// rejects, or a range on a feature that takes none is unknown, and fails;
/// a length the check cannot size (`calc()`, `vw`) is in doubt, as are the
/// features whose values the check does not follow.
fn media_feature_test(name: &str, query: FeatureQuery) -> MediaTest {
    let fixed = MediaTest::Fixed;
    let keyword = |query: FeatureQuery| match query {
        FeatureQuery::Plain([Token::Ident(word)]) => Some(word.to_ascii_lowercase()),
        _ => None,
    };
    // A discrete feature: its outcome in a boolean context, and for each
    // value it takes.
    let discrete = |boolean: Outcome, values: &[(&str, Outcome)]| match query {
        FeatureQuery::Boolean => fixed(boolean),
        FeatureQuery::Range(..) => fixed(Outcome::UNKNOWN),
        FeatureQuery::Plain(_) => fixed(
            keyword(query)
                .and_then(|word| values.iter().find(|(value, _)| *value == word))
                .map_or(Outcome::UNKNOWN, |(_, outcome)| *outcome),
        ),
    };
    let (comparison, value) = match query {
        FeatureQuery::Boolean => (Comparison::Equal, None),
        FeatureQuery::Plain(value) => (Comparison::Equal, Some(value)),
        FeatureQuery::Range(comparison, value) => (comparison, Some(value)),
    };
    let sized = |axis: Axis, size: Result<f64, Outcome>| match size {
        Ok(size) => MediaTest::Size(axis, comparison, size),
        Err(outcome) => fixed(outcome),
    };
    let between = |bound: Result<f64, Outcome>, (low, high): (f64, f64)| {
        fixed(match bound {
            Ok(bound) => match (comparison.holds(low, bound), comparison.holds(high, bound)) {
                _ if matches!(comparison, Comparison::Equal) => {
                    if (low..=high).contains(&bound) {
                        Outcome::VARIES
                    } else {
                        Outcome::FAILS
                    }
                }
                (true, true) => Outcome::HOLDS,
                (false, false) => Outcome::FAILS,
                _ => Outcome::VARIES,
            },
            Err(outcome) => outcome,
        })
    };
    let varies = Outcome::VARIES;
    match name {
        "width" | "height" | "device-width" | "device-height" => {
            let axis = if name.ends_with("width") {
                Axis::Width
            } else {
                Axis::Height
            };
            match value {
                None => fixed(Outcome::HOLDS),
                Some(value) => sized(axis, media_length(value)),
            }
        }
        "aspect-ratio" | "device-aspect-ratio" => match value {
            None => fixed(Outcome::HOLDS),
            Some(value) => sized(Axis::Ratio, media_ratio(value)),
        },
        "resolution" | "-webkit-device-pixel-ratio" => match value {
            None => fixed(Outcome::HOLDS),
            Some(value) => between(media_resolution(value, name == "resolution"), PIXEL_RATIOS),
        },
        "color" | "color-index" | "monochrome" => {
            let bits = if name == "color" { COLOR_BITS } else { 0.0 };
            match value {
                None => fixed(Outcome::of(bits > 0.0)),
                Some(value) => between(media_integer(value), (bits, bits)),
            }
        }
        "orientation" => match (query, keyword(query).as_deref()) {
            (FeatureQuery::Boolean, _) => fixed(Outcome::HOLDS),
            (_, Some("portrait")) => MediaTest::Portrait(true),
            (_, Some("landscape")) => MediaTest::Portrait(false),
            _ => fixed(Outcome::UNKNOWN),
        },
        "grid" => match (query, media_integer(value.unwrap_or(&[]))) {
            (FeatureQuery::Boolean, _) => fixed(Outcome::FAILS),
            (FeatureQuery::Plain(_), Ok(grid)) if grid == 0.0 || grid == 1.0 => {
                fixed(Outcome::of(grid == 0.0))
            }
            _ => fixed(Outcome::UNKNOWN),
        },
        "-webkit-transform-3d" => match (query, media_integer(value.unwrap_or(&[]))) {
            (FeatureQuery::Boolean, _) => fixed(Outcome::HOLDS),
            (FeatureQuery::Plain(_), Ok(on)) if on == 0.0 || on == 1.0 => {
                fixed(Outcome::of(on == 1.0))
            }
            _ => fixed(Outcome::UNKNOWN),
        },
        "hover" | "any-hover" => discrete(varies, &[("none", varies), ("hover", varies)]),
        "pointer" | "any-pointer" => discrete(
            varies,
            &[("none", varies), ("coarse", varies), ("fine", varies)],
        ),
        "prefers-reduced-motion" | "prefers-reduced-transparency" => {
            discrete(varies, &[("no-preference", varies), ("reduce", varies)])
        }
        "prefers-contrast" => discrete(
            varies,
            &[
                ("no-preference", varies),
                ("more", varies),
                ("less", varies),
                ("custom", varies),
            ],
        ),
        "prefers-color-scheme" => discrete(Outcome::DOUBT, &[("light", varies), ("dark", varies)]),
        "forced-colors" => discrete(varies, &[("none", varies), ("active", varies)]),
        "scripting" => discrete(
            varies,
            &[
                ("none", varies),
                ("enabled", varies),
                ("initial-only", Outcome::FAILS),
            ],
        ),
        "update" => discrete(
            Outcome::HOLDS,
            &[("none", Outcome::FAILS), ("slow", varies), ("fast", varies)],
        ),
        "overflow-block" => discrete(
            Outcome::HOLDS,
            &[
                ("none", Outcome::FAILS),
                ("scroll", Outcome::HOLDS),
                ("paged", Outcome::FAILS),
            ],
        ),
        "overflow-inline" => discrete(
            Outcome::HOLDS,
            &[("none", Outcome::FAILS), ("scroll", Outcome::HOLDS)],
        ),
        "color-gamut" => discrete(
            Outcome::HOLDS,
            &[
                ("srgb", Outcome::HOLDS),
                ("p3", varies),
                ("rec2020", varies),
            ],
        ),
        "dynamic-range" => discrete(
            Outcome::HOLDS,
            &[("standard", Outcome::HOLDS), ("high", varies)],
        ),
        "display-mode" => discrete(
            Outcome::DOUBT,
            &[
                ("browser", varies),
                ("fullscreen", varies),
                ("standalone", varies),
                ("minimal-ui", varies),
                ("picture-in-picture", varies),
                ("window-controls-overlay", varies),
                ("borderless", varies),
                ("tabbed", varies),
            ],
        ),
        "scan"
        | "video-dynamic-range"
        | "prefers-reduced-data"
        | "horizontal-viewport-segments"
        | "vertical-viewport-segments"
        | "device-posture" => fixed(Outcome::DOUBT),
        _ => fixed(Outcome::UNKNOWN),
    }
}

/// A number and its unit, as a numeric token holds them.
fn numeric_parts(text: &str) -> Option<(f64, String)> {
    let split = text
        .find(|character: char| character.is_ascii_alphabetic() || character == '%')
        .unwrap_or(text.len());
    let number = text[..split].parse().ok()?;
    Some((number, text[split..].to_ascii_lowercase()))
}

/// A length in a media feature, in CSS pixels: absolute units, and ems and
/// rems of the 16-pixel font a reader starts from. Units that follow the
/// viewport or the font's shape, and `calc()`, are in doubt; anything else
/// Chromium rejects.
fn media_length(value: &[Token]) -> Result<f64, Outcome> {
    match value {
        [Token::Numeric(text)] => {
            let (number, unit) = numeric_parts(text).ok_or(Outcome::UNKNOWN)?;
            let scale = match unit.as_str() {
                "" if number == 0.0 => 1.0,
                "px" => 1.0,
                "em" | "rem" | "pc" => 16.0,
                "pt" => 4.0 / 3.0,
                "in" => 96.0,
                "cm" => 96.0 / 2.54,
                "mm" => 96.0 / 25.4,
                "q" => 96.0 / 101.6,
                "ex" | "ch" | "ic" | "cap" | "lh" | "rlh" | "rex" | "rch" | "ric" | "rcap" => {
                    return Err(Outcome::DOUBT)
                }
                unit if unit.contains('v') => return Err(Outcome::DOUBT),
                _ => return Err(Outcome::UNKNOWN),
            };
            Ok(number * scale)
        }
        [Token::Function(_), ..] => Err(Outcome::DOUBT),
        _ => Err(Outcome::UNKNOWN),
    }
}

/// A ratio in a media feature (`16/9`, or one number).
fn media_ratio(value: &[Token]) -> Result<f64, Outcome> {
    let parts: Vec<&Token> = value
        .iter()
        .filter(|token| **token != Token::Whitespace)
        .collect();
    let number = |token: &Token| match token {
        Token::Numeric(text) => text.parse::<f64>().ok(),
        _ => None,
    };
    let ratio = match parts.as_slice() {
        [single] => number(single),
        [first, Token::Delim('/'), second] => number(first)
            .zip(number(second))
            .map(|(first, second)| first / second),
        _ => None,
    };
    ratio
        .filter(|ratio| ratio.is_finite() && *ratio >= 0.0)
        .ok_or(Outcome::UNKNOWN)
}

/// A resolution in a media feature, in device pixels to the CSS pixel: with
/// its unit (`2dppx`, `2x`, `192dpi`), or a bare number where the feature
/// takes one (`-webkit-device-pixel-ratio`).
fn media_resolution(value: &[Token], units: bool) -> Result<f64, Outcome> {
    let [Token::Numeric(text)] = value else {
        return Err(Outcome::UNKNOWN);
    };
    let (number, unit) = numeric_parts(text).ok_or(Outcome::UNKNOWN)?;
    match (units, unit.as_str()) {
        (true, "dppx" | "x") | (false, "") => Ok(number),
        (true, "dpi") => Ok(number / 96.0),
        (true, "dpcm") => Ok(number * 2.54 / 96.0),
        _ => Err(Outcome::UNKNOWN),
    }
}

/// A whole number in a media feature.
fn media_integer(value: &[Token]) -> Result<f64, Outcome> {
    match value {
        [Token::Numeric(text)] => text
            .parse::<i64>()
            .map(|number| number as f64)
            .map_err(|_| Outcome::UNKNOWN),
        _ => Err(Outcome::UNKNOWN),
    }
}

// ---------------------------------------------------------------------------
// Feature queries

/// `display` keywords Chromium accepts alone.
const CHROMIUM_DISPLAY_KEYWORDS: [&str; 34] = [
    "block",
    "inline",
    "inline-block",
    "flex",
    "inline-flex",
    "grid",
    "inline-grid",
    "flow",
    "flow-root",
    "contents",
    "none",
    "table",
    "inline-table",
    "table-row-group",
    "table-header-group",
    "table-footer-group",
    "table-row",
    "table-cell",
    "table-column-group",
    "table-column",
    "table-caption",
    "list-item",
    "ruby",
    "ruby-text",
    "math",
    "-webkit-box",
    "-webkit-inline-box",
    "-webkit-flex",
    "-webkit-inline-flex",
    "inherit",
    "initial",
    "unset",
    "revert",
    "revert-layer",
];

/// Parentheses an `@supports` condition may nest before it counts as one
/// the check cannot settle.
const MAX_SUPPORTS_NESTING: usize = 16;

/// Whether an `@supports` condition holds in Chromium: `not`, `and`, and
/// `or` as written; a declaration where Chromium knows its property and
/// takes its value (see [`supports_declaration`]); and `selector()` where it
/// parses the selector (see [`supports_selector`]). Another function, or
/// anything else in parentheses, does not hold, and a condition Chromium
/// cannot parse, such as `(a) and not (b)`, voids its rule.
fn supports_condition(tokens: &[Token]) -> Applies {
    supports_condition_within(tokens, 0).unwrap_or(Applies::No)
}

fn supports_condition_within(tokens: &[Token], nesting: usize) -> Option<Applies> {
    if nesting >= MAX_SUPPORTS_NESTING {
        return Some(Applies::Doubt);
    }
    let parts = components(trim_whitespace(tokens));
    let in_parens = |part: &[Token]| supports_in_parens(part, nesting + 1);
    match parts.as_slice() {
        [not, part] if is_word(not, "not") => Some(in_parens(part)?.not()),
        [first, rest @ ..] if rest.len().is_multiple_of(2) => {
            let joined_by = |joiner: &str| {
                rest.chunks(2)
                    .all(|pair| is_word(pair[0], joiner) && !is_word(pair[1], "not"))
            };
            let operands: Option<Vec<Applies>> = std::iter::once(*first)
                .chain(rest.chunks(2).map(|pair| pair[1]))
                .map(in_parens)
                .collect();
            let operands = operands?;
            if rest.is_empty() {
                operands.first().copied()
            } else if joined_by("and") {
                operands.into_iter().min()
            } else if joined_by("or") {
                operands.into_iter().max()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A part of an `@supports` condition: a declaration or a condition in
/// parentheses, or a function.
fn supports_in_parens(part: &[Token], nesting: usize) -> Option<Applies> {
    match part.first()? {
        Token::OpenParen => {
            let inner = trim_whitespace(block_contents(part));
            if let [Token::Ident(name), rest @ ..] = inner {
                if let [Token::Colon, value @ ..] = trim_whitespace(rest) {
                    return Some(supports_declaration(name, value));
                }
            }
            Some(supports_condition_within(inner, nesting).unwrap_or(Applies::No))
        }
        Token::Function(function) => Some(match function.to_ascii_lowercase().as_str() {
            "selector" => supports_selector(block_contents(part)),
            "font-tech" | "font-format" => Applies::Doubt,
            _ => Applies::No,
        }),
        _ => None,
    }
}

/// Whether Chromium supports a declaration: a custom property with any
/// value, or a property it knows (see [`CHROMIUM_PROPERTIES`]) with a value
/// it may take. Of the values, only `display`'s are read (see
/// [`chromium_display`]); a value computed with `var()` is taken.
fn supports_declaration(name: &str, value: &[Token]) -> Applies {
    if name.starts_with("--") {
        return Applies::Yes;
    }
    let name = name.to_ascii_lowercase();
    if !CHROMIUM_PROPERTIES.split(' ').any(|known| known == name) {
        return Applies::No;
    }
    let mut value = trim_whitespace(value);
    if let [before @ .., Token::Ident(word)] = value {
        if word.eq_ignore_ascii_case("important") {
            if let [before @ .., Token::Delim('!')] = trim_whitespace(before) {
                value = trim_whitespace(before);
            }
        }
    }
    if value.is_empty() {
        return Applies::No;
    }
    let computed = value.iter().any(
        |token| matches!(token, Token::Function(function) if function.eq_ignore_ascii_case("var")),
    );
    if name != "display" || computed {
        return Applies::Yes;
    }
    let words: Option<Vec<String>> = value
        .iter()
        .filter(|token| **token != Token::Whitespace)
        .map(|token| match token {
            Token::Ident(word) => Some(word.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    match words {
        Some(words) if chromium_display(&words) => Applies::Yes,
        _ => Applies::No,
    }
}

/// Whether Chromium takes a `display` value: one of its keywords, or an
/// outer display (`block`, `inline`), an inner one (`flow`, `flow-root`,
/// `table`, `flex`, `grid`, `ruby`, `math`), and `list-item` with a flow
/// inside, each at most once.
fn chromium_display(words: &[String]) -> bool {
    if let [word] = words {
        return CHROMIUM_DISPLAY_KEYWORDS.contains(&word.as_str());
    }
    let (mut outer, mut inner, mut item) = (0, None, 0);
    for word in words {
        match word.as_str() {
            "block" | "inline" => outer += 1,
            "flow" | "flow-root" | "table" | "flex" | "grid" | "ruby" | "math" => {
                if inner.replace(word.as_str()).is_some() {
                    return false;
                }
            }
            "list-item" => item += 1,
            _ => return false,
        }
    }
    outer <= 1 && item <= 1 && (item == 0 || matches!(inner, None | Some("flow" | "flow-root")))
}

/// Whether Chromium parses the selector in `selector()`: one complex
/// selector whose pseudo-classes and pseudo-elements it knows. One with
/// another engine's prefix (`-moz-`, `-ms-`, `-o-`) it rejects; one this
/// check does not know it may know.
fn supports_selector(tokens: &[Token]) -> Applies {
    let tokens = trim_whitespace(tokens);
    if tokens.is_empty() || split_top_level(tokens, &Token::Comma).len() > 1 {
        return Applies::No;
    }
    let mut applies = Applies::Yes;
    for (at, token) in tokens.iter().enumerate() {
        if *token != Token::Colon || at > 0 && tokens[at - 1] == Token::Colon {
            continue;
        }
        let element = tokens.get(at + 1) == Some(&Token::Colon);
        let name = match tokens.get(at + if element { 2 } else { 1 }) {
            Some(Token::Ident(name) | Token::Function(name)) => name.to_ascii_lowercase(),
            _ => return Applies::No,
        };
        let known = if element {
            name.starts_with("-webkit-")
                || CHROMIUM_PSEUDO_ELEMENTS
                    .split(' ')
                    .any(|known| known == name)
        } else {
            CHROMIUM_PSEUDO_CLASSES
                .split(' ')
                .any(|known| known == name)
        };
        if ["-moz-", "-ms-", "-o-"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            return Applies::No;
        }
        if !known {
            applies = Applies::Doubt;
        }
    }
    applies
}

/// The properties Chromium 141 supports, as `@supports` tests them.
const CHROMIUM_PROPERTIES: &str = "\
    -epub-caption-side -epub-text-combine -epub-text-emphasis -epub-text-emphasis-color \
    -epub-text-emphasis-style -epub-text-orientation -epub-text-transform -epub-word-break \
    -epub-writing-mode -webkit-align-content -webkit-align-items -webkit-align-self \
    -webkit-animation -webkit-animation-delay -webkit-animation-direction \
    -webkit-animation-duration -webkit-animation-fill-mode -webkit-animation-iteration-count \
    -webkit-animation-name -webkit-animation-play-state -webkit-animation-timing-function \
    -webkit-app-region -webkit-appearance -webkit-backface-visibility -webkit-background-clip \
    -webkit-background-origin -webkit-background-size -webkit-border-after \
    -webkit-border-after-color -webkit-border-after-style -webkit-border-after-width \
    -webkit-border-before -webkit-border-before-color -webkit-border-before-style \
    -webkit-border-before-width -webkit-border-bottom-left-radius \
    -webkit-border-bottom-right-radius -webkit-border-end -webkit-border-end-color \
    -webkit-border-end-style -webkit-border-end-width -webkit-border-horizontal-spacing \
    -webkit-border-image -webkit-border-radius -webkit-border-start \
    -webkit-border-start-color -webkit-border-start-style -webkit-border-start-width \
    -webkit-border-top-left-radius -webkit-border-top-right-radius \
    -webkit-border-vertical-spacing -webkit-box-align -webkit-box-decoration-break \
    -webkit-box-direction -webkit-box-flex -webkit-box-ordinal-group -webkit-box-orient \
    -webkit-box-pack -webkit-box-reflect -webkit-box-shadow -webkit-box-sizing \
    -webkit-clip-path -webkit-column-break-after -webkit-column-break-before \
    -webkit-column-break-inside -webkit-column-count -webkit-column-gap -webkit-column-rule \
    -webkit-column-rule-color -webkit-column-rule-style -webkit-column-rule-width \
    -webkit-column-span -webkit-column-width -webkit-columns -webkit-filter -webkit-flex \
    -webkit-flex-basis -webkit-flex-direction -webkit-flex-flow -webkit-flex-grow \
    -webkit-flex-shrink -webkit-flex-wrap -webkit-font-feature-settings \
    -webkit-font-smoothing -webkit-hyphenate-character -webkit-justify-content \
    -webkit-line-break -webkit-line-clamp -webkit-locale -webkit-logical-height \
    -webkit-logical-width -webkit-margin-after -webkit-margin-before -webkit-margin-end \
    -webkit-margin-start -webkit-mask -webkit-mask-box-image -webkit-mask-box-image-outset \
    -webkit-mask-box-image-repeat -webkit-mask-box-image-slice -webkit-mask-box-image-source \
    -webkit-mask-box-image-width -webkit-mask-clip -webkit-mask-composite -webkit-mask-image \
    -webkit-mask-origin -webkit-mask-position -webkit-mask-position-x -webkit-mask-position-y \
    -webkit-mask-repeat -webkit-mask-size -webkit-max-logical-height \
    -webkit-max-logical-width -webkit-min-logical-height -webkit-min-logical-width \
    -webkit-opacity -webkit-order -webkit-padding-after -webkit-padding-before \
    -webkit-padding-end -webkit-padding-start -webkit-perspective -webkit-perspective-origin \
    -webkit-perspective-origin-x -webkit-perspective-origin-y -webkit-print-color-adjust \
    -webkit-rtl-ordering -webkit-ruby-position -webkit-shape-image-threshold \
    -webkit-shape-margin -webkit-shape-outside -webkit-tap-highlight-color \
    -webkit-text-combine -webkit-text-decorations-in-effect -webkit-text-emphasis \
    -webkit-text-emphasis-color -webkit-text-emphasis-position -webkit-text-emphasis-style \
    -webkit-text-fill-color -webkit-text-orientation -webkit-text-security \
    -webkit-text-size-adjust -webkit-text-stroke -webkit-text-stroke-color \
    -webkit-text-stroke-width -webkit-transform -webkit-transform-origin \
    -webkit-transform-origin-x -webkit-transform-origin-y -webkit-transform-origin-z \
    -webkit-transform-style -webkit-transition -webkit-transition-delay \
    -webkit-transition-duration -webkit-transition-property \
    -webkit-transition-timing-function -webkit-user-drag -webkit-user-modify \
    -webkit-user-select -webkit-writing-mode accent-color align-content align-items \
    align-self alignment-baseline all anchor-name anchor-scope animation \
    animation-composition animation-delay animation-direction animation-duration \
    animation-fill-mode animation-iteration-count animation-name animation-play-state \
    animation-range animation-range-end animation-range-start animation-timeline \
    animation-timing-function app-region appearance aspect-ratio backdrop-filter \
    backface-visibility background background-attachment background-blend-mode \
    background-clip background-color background-image background-origin background-position \
    background-position-x background-position-y background-repeat background-size \
    baseline-shift baseline-source block-size border border-block border-block-color \
    border-block-end border-block-end-color border-block-end-style border-block-end-width \
    border-block-start border-block-start-color border-block-start-style \
    border-block-start-width border-block-style border-block-width border-bottom \
    border-bottom-color border-bottom-left-radius border-bottom-right-radius \
    border-bottom-style border-bottom-width border-collapse border-color \
    border-end-end-radius border-end-start-radius border-image border-image-outset \
    border-image-repeat border-image-slice border-image-source border-image-width \
    border-inline border-inline-color border-inline-end border-inline-end-color \
    border-inline-end-style border-inline-end-width border-inline-start \
    border-inline-start-color border-inline-start-style border-inline-start-width \
    border-inline-style border-inline-width border-left border-left-color border-left-style \
    border-left-width border-radius border-right border-right-color border-right-style \
    border-right-width border-spacing border-start-end-radius border-start-start-radius \
    border-style border-top border-top-color border-top-left-radius border-top-right-radius \
    border-top-style border-top-width border-width bottom box-decoration-break box-shadow \
    box-sizing break-after break-before break-inside buffered-rendering caption-side \
    caret-animation caret-color clear clip clip-path clip-rule color color-interpolation \
    color-interpolation-filters color-rendering color-scheme column-count column-fill \
    column-gap column-rule column-rule-color column-rule-style column-rule-width column-span \
    column-width columns contain contain-intrinsic-block-size contain-intrinsic-height \
    contain-intrinsic-inline-size contain-intrinsic-size contain-intrinsic-width container \
    container-name container-type content content-visibility corner-block-end-shape \
    corner-block-start-shape corner-bottom-left-shape corner-bottom-right-shape \
    corner-bottom-shape corner-end-end-shape corner-end-start-shape corner-inline-end-shape \
    corner-inline-start-shape corner-left-shape corner-right-shape corner-shape \
    corner-start-end-shape corner-start-start-shape corner-top-left-shape \
    corner-top-right-shape corner-top-shape counter-increment counter-reset counter-set \
    cursor cx cy d direction display dominant-baseline dynamic-range-limit empty-cells \
    field-sizing fill fill-opacity fill-rule filter flex flex-basis flex-direction flex-flow \
    flex-grow flex-shrink flex-wrap float flood-color flood-opacity font font-family \
    font-feature-settings font-kerning font-optical-sizing font-palette font-size \
    font-size-adjust font-stretch font-style font-synthesis font-synthesis-small-caps \
    font-synthesis-style font-synthesis-weight font-variant font-variant-alternates \
    font-variant-caps font-variant-east-asian font-variant-emoji font-variant-ligatures \
    font-variant-numeric font-variant-position font-variation-settings font-weight \
    forced-color-adjust gap grid grid-area grid-auto-columns grid-auto-flow grid-auto-rows \
    grid-column grid-column-end grid-column-gap grid-column-start grid-gap grid-row \
    grid-row-end grid-row-gap grid-row-start grid-template grid-template-areas \
    grid-template-columns grid-template-rows height hyphenate-character hyphenate-limit-chars \
    hyphens image-orientation image-rendering initial-letter inline-size inset inset-block \
    inset-block-end inset-block-start inset-inline inset-inline-end inset-inline-start \
    interactivity interpolate-size isolation justify-content justify-items justify-self left \
    letter-spacing lighting-color line-break line-height list-style list-style-image \
    list-style-position list-style-type margin margin-block margin-block-end \
    margin-block-start margin-bottom margin-inline margin-inline-end margin-inline-start \
    margin-left margin-right margin-top marker marker-end marker-mid marker-start mask \
    mask-clip mask-composite mask-image mask-mode mask-origin mask-position mask-repeat \
    mask-size mask-type math-depth math-shift math-style max-block-size max-height \
    max-inline-size max-width min-block-size min-height min-inline-size min-width \
    mix-blend-mode object-fit object-position object-view-box offset offset-anchor \
    offset-distance offset-path offset-position offset-rotate opacity order orphans outline \
    outline-color outline-offset outline-style outline-width overflow overflow-anchor \
    overflow-block overflow-clip-margin overflow-inline overflow-wrap overflow-x overflow-y \
    overlay overscroll-behavior overscroll-behavior-block overscroll-behavior-inline \
    overscroll-behavior-x overscroll-behavior-y padding padding-block padding-block-end \
    padding-block-start padding-bottom padding-inline padding-inline-end padding-inline-start \
    padding-left padding-right padding-top page page-break-after page-break-before \
    page-break-inside page-orientation paint-order perspective perspective-origin \
    place-content place-items place-self pointer-events position position-anchor \
    position-area position-try position-try-fallbacks position-try-order position-visibility \
    print-color-adjust quotes r reading-flow reading-order resize right rotate row-gap \
    ruby-align ruby-position rx ry scale scroll-behavior scroll-initial-target scroll-margin \
    scroll-margin-block scroll-margin-block-end scroll-margin-block-start \
    scroll-margin-bottom scroll-margin-inline scroll-margin-inline-end \
    scroll-margin-inline-start scroll-margin-left scroll-margin-right scroll-margin-top \
    scroll-marker-group scroll-padding scroll-padding-block scroll-padding-block-end \
    scroll-padding-block-start scroll-padding-bottom scroll-padding-inline \
    scroll-padding-inline-end scroll-padding-inline-start scroll-padding-left \
    scroll-padding-right scroll-padding-top scroll-snap-align scroll-snap-stop \
    scroll-snap-type scroll-target-group scroll-timeline scroll-timeline-axis \
    scroll-timeline-name scrollbar-color scrollbar-gutter scrollbar-width \
    shape-image-threshold shape-margin shape-outside shape-rendering size speak stop-color \
    stop-opacity stroke stroke-dasharray stroke-dashoffset stroke-linecap stroke-linejoin \
    stroke-miterlimit stroke-opacity stroke-width tab-size table-layout text-align \
    text-align-last text-anchor text-autospace text-box text-box-edge text-box-trim \
    text-combine-upright text-decoration text-decoration-color text-decoration-line \
    text-decoration-skip-ink text-decoration-style text-decoration-thickness text-emphasis \
    text-emphasis-color text-emphasis-position text-emphasis-style text-indent \
    text-orientation text-overflow text-rendering text-shadow text-size-adjust \
    text-spacing-trim text-transform text-underline-offset text-underline-position text-wrap \
    text-wrap-mode text-wrap-style timeline-scope top touch-action transform transform-box \
    transform-origin transform-style transition transition-behavior transition-delay \
    transition-duration transition-property transition-timing-function translate unicode-bidi \
    user-select vector-effect vertical-align view-timeline view-timeline-axis \
    view-timeline-inset view-timeline-name view-transition-class view-transition-group \
    view-transition-name visibility white-space white-space-collapse widows width will-change \
    word-break word-spacing word-wrap writing-mode x y z-index zoom";

/// The pseudo-classes Chromium 141 parses, and the pseudo-elements it
/// parses after one colon.
const CHROMIUM_PSEUDO_CLASSES: &str = "\
    -webkit-any -webkit-any-link -webkit-autofill -webkit-drag -webkit-full-page-media \
    -webkit-full-screen active active-view-transition after any-link autofill before checked \
    corner-present decrement default defined dir disabled double-button empty enabled end \
    first-child first-letter first-line first-of-type focus focus-visible focus-within \
    fullscreen future has horizontal host host-context hover in-range increment indeterminate \
    invalid is lang last-child last-of-type link modal no-button not nth-child nth-last-child \
    nth-last-of-type nth-of-type only-child only-of-type open optional out-of-range past \
    picture-in-picture placeholder-shown popover-open read-only read-write required root \
    scope single-button start state target target-current user-invalid user-valid valid \
    vertical visited where window-inactive xr-overlay";

/// The pseudo-elements Chromium 141 parses, besides its own `-webkit-`
/// ones.
const CHROMIUM_PSEUDO_ELEMENTS: &str = "\
    -webkit-inner-spin-button -webkit-input-placeholder -webkit-meter-bar \
    -webkit-progress-bar -webkit-scrollbar -webkit-search-cancel-button after backdrop before \
    checkmark column cue details-content file-selector-button first-letter first-line \
    grammar-error highlight marker part picker picker-icon placeholder scroll-button \
    scroll-marker selection slotted spelling-error target-text view-transition";

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
    /// For a flex box: whether its items stand in a column, or a row in
    /// reverse (`flex-direction`).
    FlexDirection,
    /// For a flex box: whether its items may wrap onto more lines.
    FlexWrap,
    /// For an old flexible box (`-webkit-box`): whether its items stand in
    /// a column (`-webkit-box-orient`).
    BoxOrient,
    /// For a flex box: whether `justify-content` spreads its items along
    /// the line (`space-between`, `space-around`, `space-evenly`).
    JustifyContent,
    /// For a flex or grid box: whether a gap stands between its columns
    /// (`column-gap`, `gap`).
    ColumnGap,
    /// For an old flexible box: whether it clamps its lines
    /// (`-webkit-line-clamp`), which lays its children out as lines.
    LineClamp,
    /// Whether a box's left or right margin or padding is wider than none,
    /// setting it apart from the flex or grid items beside it, or the text
    /// beside an inline box laying out items.
    MarginLeft,
    MarginRight,
    PaddingLeft,
    PaddingRight,
    /// For a flex item: whether its width, or its flex basis where one is
    /// set (`flex-basis`, `flex`), takes a whole line (see
    /// [`full_line`]); `FlexBasisAuto` for a basis that takes the width.
    Width,
    FlexBasis,
    FlexBasisAuto,
    /// A custom property (`--gutter`), read for the margins, padding, and
    /// gaps that take their size from it: whether it is a length wider
    /// than none.
    Custom,
    /// Whether a box is a size container, whose size `@container` rules
    /// query (`container-type`, `container`).
    Container,
}

impl Property {
    /// A margin or padding, or a flex item's width or basis, which only a
    /// flex or grid item, and an inline box laying such items out, is read
    /// for.
    fn spaces(self) -> bool {
        matches!(
            self,
            Property::MarginLeft
                | Property::MarginRight
                | Property::PaddingLeft
                | Property::PaddingRight
                | Property::Width
                | Property::FlexBasis
                | Property::FlexBasisAuto
        )
    }
}

/// How a box lays out its children, as `display` sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// In lines and blocks.
    Flow,
    /// As flex items.
    Flex,
    /// As the items of an old flexible box (`-webkit-box`).
    Box,
    /// As grid items.
    Grid,
    /// It has no box; its children take its place (`contents`).
    Contents,
    /// Not known until run time.
    Unknown,
}

/// What a `content` declaration makes a `::before` or `::after` box show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Generated {
    /// No box (`none`, `normal`).
    Nothing,
    /// An empty box.
    Plain,
    /// A box showing what carries no meaning of its own: white space, quote
    /// marks, bullets, and other ornaments, or an image. Between digits it
    /// still keeps them apart.
    Ornament,
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

/// Where what generated content shows reads as part of an amount, or keeps
/// two numbers apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sign {
    /// Anything shown, a space or an ornament, a comma and a space, a slash
    /// or a colon: between digits, where it keeps two numbers apart ("12,
    /// 15", "12:30") that run together without it.
    Between,
    /// A decimal point or thousands separator ending the box: part of a
    /// number where a digit follows it directly, as "1" and ".99" read
    /// "1.99"; after a number, as in "1.", it only closes it.
    Separator,
    /// Hyphens or dashes alone ("-", "– "), and whether white space leads
    /// and trails them: a minus beside the digits they touch, as
    /// [`Sign::Amount`] is, and otherwise a bullet before a list item's
    /// text, or a separator after a number (see [`PseudoBox`]).
    Dash { leading: bool, trailing: bool },
    /// A minus or plus, a parenthesis, a percent or currency sign, or a
    /// dash ("−", "(", "%", "$"): beside digits on either side.
    Amount,
}

impl Sign {
    /// Whether it reads with the digits of an amount beside it.
    fn of_amount(self) -> bool {
        matches!(self, Sign::Dash { .. } | Sign::Amount)
    }
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
    /// flow; for a property of flex or grid items, or a margin or padding,
    /// what it says (see [`Property`]). `Maybe` for a value not known until
    /// run time; `None` for a declaration a reader ignores.
    flow: Option<Tri>,
    /// For `display`, how the box lays out its children.
    layout: Option<Layout>,
    /// For `float`, whether the box floats to the start of the line (`left`
    /// or `inline-start`), where a drop cap stands.
    side: Option<Tri>,
    /// For `content`, what the box shows; `None` when not known until run
    /// time.
    generated: Option<Generated>,
    /// For a custom property (`--gutter`), its name; for a margin,
    /// padding, or gap that takes its size from one (`var(--gutter)`,
    /// `calc(var(--gutter) * .5)`), that one's, `flow` then telling what
    /// it says where the custom property is not set (see
    /// [`Cascade::resolved`]). Names are hashed ([`custom_name`]).
    var: Option<u64>,
}

/// The key a custom property's name is known by: its name, whose case
/// counts, hashed.
fn custom_name(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// How a length takes its size from a custom property (see
/// [`var_sign`]).
#[derive(Clone, Copy)]
enum VarSign {
    /// It is wider than none where the property's value is: the property,
    /// and what it says where the property is not set (its fallback's, or
    /// none).
    Takes(u64, Tri),
    /// It is none or less however the property is set (a negative multiple).
    Never,
}

/// How a length written with a custom property takes its size from it:
/// `var(--x)`, with or without a fallback, and `calc()` multiplying or
/// dividing it by a number; `None` for anything else.
fn var_sign(part: &[Token]) -> Option<VarSign> {
    let (Token::Function(function), true) = (part.first()?, true) else {
        return None;
    };
    let (arguments, end) = block_at(part, 0);
    if end != part.len() {
        return None;
    }
    let arguments = trim_whitespace(arguments);
    if function.eq_ignore_ascii_case("var") {
        let [Token::Ident(name), rest @ ..] = arguments else {
            return None;
        };
        if !name.starts_with("--") {
            return None;
        }
        let unset = match trim_whitespace(rest) {
            [] => Tri::No,
            [Token::Comma, fallback @ ..] => positive_length(trim_whitespace(fallback))?,
            _ => return None,
        };
        return Some(VarSign::Takes(custom_name(name), unset));
    }
    if !function.eq_ignore_ascii_case("calc") {
        return None;
    }
    let terms: Vec<&[Token]> = {
        let mut terms = Vec::new();
        let mut index = 0;
        while index < arguments.len() {
            let end = skip_component(arguments, index);
            if arguments[index] != Token::Whitespace {
                terms.push(&arguments[index..end]);
            }
            index = end;
        }
        terms
    };
    let number = |term: &[Token]| match term {
        [Token::Numeric(number)] => number.parse::<f64>().ok(),
        _ => None,
    };
    let (variable, factor) = match terms.as_slice() {
        [first, [Token::Delim(operator @ ('*' | '/'))], second] => {
            match (number(first), number(second)) {
                (None, Some(factor)) => (*first, factor),
                (Some(factor), None) if *operator == '*' => (*second, factor),
                _ => return None,
            }
        }
        _ => return None,
    };
    match var_sign(variable)? {
        VarSign::Takes(..) if factor < 0.0 => Some(VarSign::Never),
        VarSign::Takes(_, _) if factor == 0.0 => Some(VarSign::Never),
        sign => Some(sign),
    }
}

/// `display` keywords that lay a box's children out as flex items, as the
/// items of an old flexible box, and as grid items.
const FLEX_DISPLAY_KEYWORDS: [&str; 6] = [
    "flex",
    "inline-flex",
    "-webkit-flex",
    "-webkit-inline-flex",
    "-ms-flexbox",
    "-ms-inline-flexbox",
];
const BOX_DISPLAY_KEYWORDS: [&str; 4] = [
    "-webkit-box",
    "-webkit-inline-box",
    "-moz-box",
    "-moz-inline-box",
];
const GRID_DISPLAY_KEYWORDS: [&str; 4] = ["grid", "inline-grid", "-ms-grid", "-ms-inline-grid"];

/// Signs an amount reads by, which generated content may add to it: a
/// minus or plus, parentheses, percent and per-mille signs, with their
/// full-width, small, superscript, and subscript forms, the triangles
/// Japanese accounts mark a loss with, the hyphens and dashes set as a
/// minus or between figures, and currency signs.
fn amount_sign(character: char) -> bool {
    matches!(
        character,
        '\u{2212}'
            | '-'
            | '+'
            | '('
            | ')'
            | '%'
            | '\u{b1}'
            | '\u{2213}'
            | '\u{2030}'
            | '\u{2031}'
            | '\u{66a}'
            | '\u{2796}'
            | '\u{207a}'
            | '\u{207b}'
            | '\u{207d}'
            | '\u{207e}'
            | '\u{208a}'
            | '\u{208b}'
            | '\u{208d}'
            | '\u{208e}'
            | '\u{fe59}'
            | '\u{fe5a}'
            | '\u{fe62}'
            | '\u{fe63}'
            | '\u{fe6a}'
            | '\u{ff05}'
            | '\u{ff08}'
            | '\u{ff09}'
            | '\u{ff0b}'
            | '\u{ff0d}'
            | '\u{25b2}'
            | '\u{25b3}'
            | '\u{2010}'..='\u{2013}'
    ) || currency_sign(character)
}

/// Unicode's currency signs (general category Sc): `$`, `¢` to `¥`, the
/// currency block (`€`, `₹`, and the rest), and the signs of other scripts,
/// such as `฿` and `֏`, and their full-width and small forms.
fn currency_sign(character: char) -> bool {
    matches!(
        character,
        '$' | '\u{a2}'..='\u{a5}'
            | '\u{58f}'
            | '\u{60b}'
            | '\u{7fe}'
            | '\u{7ff}'
            | '\u{9f2}'
            | '\u{9f3}'
            | '\u{9fb}'
            | '\u{af1}'
            | '\u{bf9}'
            | '\u{e3f}'
            | '\u{17db}'
            | '\u{20a0}'..='\u{20cf}'
            | '\u{a838}'
            | '\u{fdfc}'
            | '\u{fe69}'
            | '\u{ff04}'
            | '\u{ffe0}'
            | '\u{ffe1}'
            | '\u{ffe5}'
            | '\u{ffe6}'
            | '\u{11fdd}'..='\u{11fe0}'
            | '\u{1e2ff}'
            | '\u{1ecb0}'
    )
}

/// Decimal points and thousands separators: the full stop and comma, their
/// full-width forms, and the Arabic separators.
fn decimal_separator(character: char) -> bool {
    matches!(
        character,
        '.' | ',' | '\u{66b}' | '\u{66c}' | '\u{ff0c}' | '\u{ff0e}'
    )
}

/// Characters a reader shows nothing for: zero-width spaces and joiners,
/// directional marks, the word joiner, and the soft hyphen.
fn invisible(character: char) -> bool {
    matches!(
        character,
        '\u{ad}' | '\u{34f}' | '\u{200b}'..='\u{200f}' | '\u{2060}'..='\u{2064}' | '\u{feff}'
    )
}

/// Whether text starts an amount: after white space, a digit, or signs and
/// a decimal point before one ("$1,250", ".75").
fn starts_amount(mut text: impl Iterator<Item = char>) -> bool {
    text.find(|character| {
        !(character.is_whitespace()
            || invisible(*character)
            || amount_sign(*character)
            || decimal_separator(*character))
    })
    .is_some_and(char::is_numeric)
}

/// What a string in a `content` value shows.
fn shown_text(text: &str) -> Generated {
    let visible = || text.chars().filter(|character| !invisible(*character));
    if text.chars().any(char::is_alphanumeric) {
        Generated::Text
    } else if text.chars().any(amount_sign) {
        let dash = |character: char| matches!(character, '-' | '\u{2010}'..='\u{2013}');
        let marks: String = visible().collect();
        let dashes = marks.trim();
        if !dashes.is_empty() && dashes.chars().all(dash) {
            Generated::Sign(Sign::Dash {
                leading: marks.starts_with(char::is_whitespace),
                trailing: marks.ends_with(char::is_whitespace),
            })
        } else {
            Generated::Sign(Sign::Amount)
        }
    } else if visible().next_back().is_some_and(decimal_separator) {
        Generated::Sign(Sign::Separator)
    } else if text.contains('\n') {
        Generated::LineFeed
    } else if visible().next().is_some() {
        Generated::Ornament
    } else {
        Generated::Plain
    }
}

/// What a `content` value shows, from its tokens. A function's arguments
/// are not shown as text, and text after a `/` is the alternative text of
/// what comes before, which a reader does not show either.
fn generated_content(value: &[Token]) -> Option<Generated> {
    let mut generated = Generated::Plain;
    let mut index = 0;
    while index < value.len() {
        match &value[index] {
            Token::Ident(word) => match word.to_ascii_lowercase().as_str() {
                "none" | "normal" => return Some(Generated::Nothing),
                "initial" | "unset" | "revert" => return Some(Generated::Nothing),
                "open-quote" | "close-quote" => generated = generated.max(Generated::Ornament),
                _ => {}
            },
            Token::Str(text) => generated = generated.max(shown_text(text)),
            // An image.
            Token::Url(_) => generated = generated.max(Generated::Ornament),
            Token::Function(function) => match function.to_ascii_lowercase().as_str() {
                "counter" | "counters" | "attr" => generated = Generated::Text,
                "var" | "env" | "if" => return None,
                // An image, or a gradient.
                _ => generated = generated.max(Generated::Ornament),
            },
            Token::Delim('/') => break,
            _ => {}
        }
        index = skip_component(value, index);
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

/// The declaration of a property, named in lower case, with the tokens
/// after its name, if it sets `display`, `visibility`, `content-visibility`,
/// or `float`. A value computed at run time (`var()`, `env()`, `attr()`,
/// `if()`) may hide, so it counts as hiding.
fn parse_declaration(name: &str, rest: &[Token]) -> Option<Declaration> {
    let property = match name {
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
        // The others set no flow; `parse_declarations` reads those of flex
        // and grid items, and margins and padding.
        _ => None,
    };
    let layout = match property {
        Property::Display if computed || has(&["inherit"]) => Some(Layout::Unknown),
        Property::Display if effect == Effect::Show => Some(if has(&["contents"]) {
            Layout::Contents
        } else if has(&FLEX_DISPLAY_KEYWORDS) {
            Layout::Flex
        } else if has(&BOX_DISPLAY_KEYWORDS) {
            Layout::Box
        } else if has(&GRID_DISPLAY_KEYWORDS) {
            Layout::Grid
        } else {
            Layout::Flow
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
        layout,
        side,
        generated,
        var: None,
    })
}

/// What a declaration of flex items' layout, or a margin or padding, says
/// (see [`parse_declarations`]): one property; the left and the right of a
/// margin or padding, from one to four lengths from the top clockwise, or
/// one or two from the start; a flex box's direction and wrapping; the
/// gap between columns, after that between rows; or a flex item's basis,
/// alone or after its growth and shrinking (`flex`).
#[derive(Clone, Copy)]
enum Reads {
    One(Property, fn(&[&[Token]]) -> Option<Tri>),
    Sides(Property, Property, bool),
    FlexFlow,
    Gap,
    Basis,
    Flex,
}

/// The declarations a token run holds that the check reads. Most set one
/// property; `margin`, `padding`, and their inline forms set a box's left
/// and right, `flex-flow` a flex box's direction and wrapping.
fn parse_declarations(tokens: &[Token]) -> [Option<Declaration>; 2] {
    let [Token::Ident(name), rest @ ..] = trim_whitespace(tokens) else {
        return [None, None];
    };
    if name.starts_with("--") {
        return [custom_declaration(name, rest), None];
    }
    let name = name.to_ascii_lowercase();
    let reads = match name.as_str() {
        "flex-direction" | "-webkit-flex-direction" => {
            Reads::One(Property::FlexDirection, flex_direction)
        }
        "flex-wrap" | "-webkit-flex-wrap" => Reads::One(Property::FlexWrap, flex_wrap),
        "flex-flow" | "-webkit-flex-flow" => Reads::FlexFlow,
        "-webkit-box-orient" => Reads::One(Property::BoxOrient, box_orient),
        "justify-content" | "-webkit-justify-content" => {
            Reads::One(Property::JustifyContent, spreads)
        }
        "gap" | "grid-gap" => Reads::Gap,
        "column-gap" | "grid-column-gap" => Reads::One(Property::ColumnGap, one_positive_length),
        "-webkit-line-clamp" => Reads::One(Property::LineClamp, line_clamp),
        "margin" => Reads::Sides(Property::MarginLeft, Property::MarginRight, true),
        "padding" => Reads::Sides(Property::PaddingLeft, Property::PaddingRight, true),
        "margin-inline" => Reads::Sides(Property::MarginLeft, Property::MarginRight, false),
        "padding-inline" => Reads::Sides(Property::PaddingLeft, Property::PaddingRight, false),
        "margin-left" | "margin-inline-start" | "-webkit-margin-start" => {
            Reads::One(Property::MarginLeft, one_positive_length)
        }
        "margin-right" | "margin-inline-end" | "-webkit-margin-end" => {
            Reads::One(Property::MarginRight, one_positive_length)
        }
        "padding-left" | "padding-inline-start" | "-webkit-padding-start" => {
            Reads::One(Property::PaddingLeft, one_positive_length)
        }
        "padding-right" | "padding-inline-end" | "-webkit-padding-end" => {
            Reads::One(Property::PaddingRight, one_positive_length)
        }
        "width" => Reads::One(Property::Width, one_full_line),
        "container-type" => Reads::One(Property::Container, size_container),
        "container" => Reads::One(Property::Container, container_shorthand),
        "flex-basis" | "-webkit-flex-basis" => Reads::Basis,
        "flex" | "-webkit-flex" | "-ms-flex" => Reads::Flex,
        _ => return [parse_declaration(&name, rest), None],
    };
    let [Token::Colon, value @ ..] = trim_whitespace(rest) else {
        return [None, None];
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
    let computed = value.iter().any(|token| {
        matches!(token, Token::Function(function) if ["var", "env", "attr", "if"]
            .iter()
            .any(|name| function.eq_ignore_ascii_case(name)))
    });
    let parts = split_top_level(value, &Token::Whitespace);
    let declare = |property: Property, says: Option<Tri>| {
        let flow = if computed { Tri::Maybe } else { says? };
        Some(Declaration {
            property,
            effect: Effect::Neutral,
            important,
            flow: Some(flow),
            layout: None,
            side: None,
            generated: None,
            var: None,
        })
    };
    // A margin, padding, or gap written with a custom property takes its
    // size from it.
    let spaced = |property: Property, part: &[Token]| match var_sign(part) {
        Some(VarSign::Takes(name, unset)) => Some(Declaration {
            property,
            effect: Effect::Neutral,
            important,
            flow: Some(unset),
            layout: None,
            side: None,
            generated: None,
            var: Some(name),
        }),
        Some(VarSign::Never) => Some(Declaration {
            property,
            effect: Effect::Neutral,
            important,
            flow: Some(Tri::No),
            layout: None,
            side: None,
            generated: None,
            var: None,
        }),
        None => declare(property, positive_length(part)),
    };
    match reads {
        Reads::One(
            property @ (Property::MarginLeft
            | Property::MarginRight
            | Property::PaddingLeft
            | Property::PaddingRight
            | Property::ColumnGap),
            _,
        ) => match parts.as_slice() {
            [part] => [spaced(property, part), None],
            _ => [None, None],
        },
        Reads::One(property, says) => [declare(property, says(&parts)), None],
        Reads::Sides(left, right, clockwise) => {
            let (start, end) = match (clockwise, parts.as_slice()) {
                (true, [all]) | (false, [all]) => (all, all),
                (true, [_, across] | [_, across, _]) => (across, across),
                (true, [_, right, _, left]) => (left, right),
                (false, [start, end]) => (start, end),
                _ => return [None, None],
            };
            [spaced(left, start), spaced(right, end)]
        }
        Reads::FlexFlow => [
            declare(Property::FlexDirection, flex_direction(&parts)),
            declare(Property::FlexWrap, flex_wrap(&parts)),
        ],
        Reads::Gap => match parts.last() {
            Some(gap) => [spaced(Property::ColumnGap, gap), None],
            None => [None, None],
        },
        Reads::Basis => match keyword(&parts).as_deref() {
            Some("auto" | "content") => [declare(Property::FlexBasisAuto, Some(Tri::No)), None],
            _ => [declare(Property::FlexBasis, one_full_line(&parts)), None],
        },
        // `flex: none`, `auto`, and `initial` take the width; a basis
        // after the numbers, or 0% where none is written (`flex: 1`).
        Reads::Flex => match keyword(&parts).as_deref() {
            Some("none" | "auto" | "initial") => {
                [declare(Property::FlexBasisAuto, Some(Tri::No)), None]
            }
            _ => {
                let number = |part: &&[Token]| matches!(part, [Token::Numeric(number)] if number.parse::<f64>().is_ok());
                match parts.iter().position(|part| !number(part)) {
                    None if !parts.is_empty() && parts.len() <= 2 => {
                        [declare(Property::FlexBasis, Some(Tri::No)), None]
                    }
                    Some(at) if at == parts.len() - 1 && at <= 2 => {
                        match keyword(&parts[at..]).as_deref() {
                            Some("auto" | "content") => {
                                [declare(Property::FlexBasisAuto, Some(Tri::No)), None]
                            }
                            _ => [
                                declare(Property::FlexBasis, one_full_line(&parts[at..])),
                                None,
                            ],
                        }
                    }
                    _ => [None, None],
                }
            }
        },
    }
}

/// A custom property's declaration (`--gutter: 1.5rem`): whether its value
/// is a length wider than none, `Maybe` for anything else.
fn custom_declaration(name: &str, rest: &[Token]) -> Option<Declaration> {
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
    let wide = match value {
        [Token::Numeric(_)] => positive_length(value).unwrap_or(Tri::Maybe),
        _ => Tri::Maybe,
    };
    Some(Declaration {
        property: Property::Custom,
        effect: Effect::Neutral,
        important,
        flow: Some(wide),
        layout: None,
        side: None,
        generated: None,
        var: Some(custom_name(name)),
    })
}

/// The keyword a value holds, lowercased, if it is one keyword.
fn keyword(parts: &[&[Token]]) -> Option<String> {
    match parts {
        [[Token::Ident(word)]] => Some(word.to_ascii_lowercase()),
        _ => None,
    }
}

/// Whether a `flex-direction` or `flex-flow` value sets the items in a
/// column or in reverse: `Maybe` where it takes the parent's.
fn flex_direction(parts: &[&[Token]]) -> Option<Tri> {
    let mut direction = Tri::No;
    for part in parts {
        let [Token::Ident(word)] = part else {
            return None;
        };
        match word.to_ascii_lowercase().as_str() {
            "row" | "initial" | "unset" => {}
            "row-reverse" | "column" | "column-reverse" => direction = Tri::Yes,
            "inherit" | "revert" | "revert-layer" => direction = Tri::Maybe,
            "nowrap" | "wrap" | "wrap-reverse" => {}
            _ => return None,
        }
    }
    Some(direction)
}

/// Whether a `flex-wrap` or `flex-flow` value lets the items wrap.
fn flex_wrap(parts: &[&[Token]]) -> Option<Tri> {
    let mut wraps = Tri::No;
    for part in parts {
        let [Token::Ident(word)] = part else {
            return None;
        };
        match word.to_ascii_lowercase().as_str() {
            "nowrap" | "initial" | "unset" => {}
            "wrap" | "wrap-reverse" => wraps = Tri::Yes,
            "inherit" | "revert" | "revert-layer" => wraps = Tri::Maybe,
            "row" | "row-reverse" | "column" | "column-reverse" => {}
            _ => return None,
        }
    }
    Some(wraps)
}

/// Whether a `container-type` value makes a box a size container.
fn size_container(parts: &[&[Token]]) -> Option<Tri> {
    let mut sized = Tri::No;
    for part in parts {
        let [Token::Ident(word)] = part else {
            return None;
        };
        match word.to_ascii_lowercase().as_str() {
            "size" | "inline-size" => sized = Tri::Yes,
            "normal" | "scroll-state" | "anchored" | "initial" | "unset" => {}
            "inherit" | "revert" | "revert-layer" => sized = sized.max(Tri::Maybe),
            _ => return None,
        }
    }
    Some(sized)
}

/// Whether a `container` value, names and then `/` and a type, makes a box
/// a size container.
fn container_shorthand(parts: &[&[Token]]) -> Option<Tri> {
    let tokens: Vec<Token> = parts.iter().flat_map(|part| part.iter().cloned()).collect();
    match tokens.iter().position(|token| *token == Token::Delim('/')) {
        Some(slash) => {
            let kind = &tokens[slash + 1..];
            let parts: Vec<&[Token]> = kind.chunks(1).collect();
            size_container(&parts)
        }
        None => match keyword(parts).as_deref() {
            Some("inherit" | "revert" | "revert-layer") => Some(Tri::Maybe),
            _ => Some(Tri::No),
        },
    }
}

/// Whether a `-webkit-box-orient` value stacks the items.
fn box_orient(parts: &[&[Token]]) -> Option<Tri> {
    match keyword(parts)?.as_str() {
        "horizontal" | "inline-axis" | "initial" | "unset" => Some(Tri::No),
        "vertical" | "block-axis" => Some(Tri::Yes),
        "inherit" | "revert" | "revert-layer" => Some(Tri::Maybe),
        _ => None,
    }
}

/// Whether a `justify-content` value spreads the items along the line.
fn spreads(parts: &[&[Token]]) -> Option<Tri> {
    let words: Vec<String> = parts
        .iter()
        .map(|part| match part {
            [Token::Ident(word)] => Some(word.to_ascii_lowercase()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let spread = |word: &str| matches!(word, "space-between" | "space-around" | "space-evenly");
    if words.iter().any(|word| spread(word)) {
        Some(Tri::Yes)
    } else if words
        .iter()
        .any(|word| matches!(word.as_str(), "inherit" | "revert" | "revert-layer"))
    {
        Some(Tri::Maybe)
    } else {
        Some(Tri::No)
    }
}

/// Whether a `-webkit-line-clamp` value clamps the lines.
fn line_clamp(parts: &[&[Token]]) -> Option<Tri> {
    match parts {
        [[Token::Numeric(number)]] => {
            let lines: f64 = number.parse().ok()?;
            Some(if lines > 0.0 { Tri::Yes } else { Tri::No })
        }
        _ => match keyword(parts)?.as_str() {
            "none" | "initial" | "unset" => Some(Tri::No),
            "inherit" | "revert" | "revert-layer" => Some(Tri::Maybe),
            _ => None,
        },
    }
}

/// Whether a margin, padding, or gap is wider than none: a positive length
/// or percentage; `Maybe` for `auto`, which a flex item's free space may
/// fill, and for a value computed from others (`calc()`).
fn positive_length(part: &[Token]) -> Option<Tri> {
    match part {
        [Token::Numeric(number)] => {
            let split = number
                .find(|character: char| character.is_ascii_alphabetic() || character == '%')
                .unwrap_or(number.len());
            let width: f64 = number[..split].parse().ok()?;
            Some(if width > 0.0 { Tri::Yes } else { Tri::No })
        }
        [Token::Ident(word)] => match word.to_ascii_lowercase().as_str() {
            "normal" | "initial" | "unset" => Some(Tri::No),
            "auto" | "inherit" | "revert" | "revert-layer" => Some(Tri::Maybe),
            _ => None,
        },
        [Token::Function(_), ..] => Some(Tri::Maybe),
        _ => None,
    }
}

/// [`positive_length`] for a value that must be one length.
fn one_positive_length(parts: &[&[Token]]) -> Option<Tri> {
    match parts {
        [part] => positive_length(part),
        _ => None,
    }
}

/// Whether a flex item's width or basis takes a whole line of wrapping
/// items: a percentage of 100 or more. A smaller one, `auto`, and zero do
/// not; a length, whose share of the line is not known, and a value
/// computed from others (`calc()`) or sized to the content (`max-content`)
/// may.
fn full_line(part: &[Token]) -> Option<Tri> {
    match part {
        [Token::Numeric(number)] => match number.strip_suffix('%') {
            Some(percent) => {
                let share: f64 = percent.parse().ok()?;
                Some(if share >= 100.0 { Tri::Yes } else { Tri::No })
            }
            None => match positive_length(part)? {
                Tri::No => Some(Tri::No),
                _ => Some(Tri::Maybe),
            },
        },
        [Token::Ident(word)] => match word.to_ascii_lowercase().as_str() {
            "auto" | "initial" | "unset" => Some(Tri::No),
            "min-content"
            | "max-content"
            | "fit-content"
            | "stretch"
            | "-webkit-fill-available"
            | "inherit"
            | "revert"
            | "revert-layer" => Some(Tri::Maybe),
            _ => None,
        },
        [Token::Function(_), ..] => Some(Tri::Maybe),
        _ => None,
    }
}

/// [`full_line`] for a value that must be one width.
fn one_full_line(parts: &[&[Token]]) -> Option<Tri> {
    match parts {
        [part] => full_line(part),
        _ => None,
    }
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
    /// `:is()`, `:where()`, and `&`, which the rules nested in a rule share
    /// (see [`Nest`]).
    Is(Rc<[ComplexSelector]>),
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
    /// Its compound selectors, those of the lists in its pseudo-classes
    /// counted (see [`MAX_SELECTOR_WEIGHT`]).
    weight: u32,
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
            weight: 1,
        }
    }
}

fn add_specificity(total: &mut (u32, u32, u32), other: (u32, u32, u32)) {
    total.0 += other.0;
    total.1 += other.1;
    total.2 += other.2;
}

/// What `&` stands for in the selectors of the rules nested in a style
/// rule: the elements the rule's selectors match, a selector of a
/// pseudo-element matching none, with the highest specificity among them,
/// as `:is()` takes them. In the style rules of an `@scope` rule, `&` and
/// `:scope` stand for the scope's root, and add no specificity where they
/// are left implicit (`scope`).
#[derive(Clone)]
struct Nest {
    parent: Rc<[ComplexSelector]>,
    specificity: (u32, u32, u32),
    scope: bool,
}

/// Parse a selector list; `nest` says what `&` stands for in a nested
/// rule's.
fn parse_selector_list(
    tokens: &[Token],
    nesting: usize,
    nest: Option<&Nest>,
) -> Vec<ComplexSelector> {
    let parts = split_top_level(tokens, &Token::Comma);
    if parts.len() > MAX_SELECTORS_PER_RULE {
        return vec![ComplexSelector::undecided()];
    }
    parts
        .into_iter()
        .map(|part| parse_complex(trim_whitespace(part), nesting, nest))
        .collect()
}

/// A nested rule's selectors made whole, as a reader reads them: one that
/// does not name the rule it is nested in (`&`, or in a scope `:scope`) is
/// one inside it, as though `& ` came first (`p` reads `& p`, and `> p`
/// reads `& > p`).
fn nested_selectors(prelude: &[Token], scope: bool) -> Vec<Token> {
    let mut tokens = Vec::with_capacity(prelude.len() + 2);
    for (at, part) in split_top_level(prelude, &Token::Comma)
        .into_iter()
        .enumerate()
    {
        if at > 0 {
            tokens.push(Token::Comma);
        }
        let part = trim_whitespace(part);
        let names_scope = scope
            && part.windows(2).any(|pair| {
                matches!(pair, [Token::Colon, Token::Ident(name)] if name.eq_ignore_ascii_case("scope"))
            });
        if !part.is_empty() && !part.contains(&Token::Delim('&')) && !names_scope {
            tokens.extend([Token::Delim('&'), Token::Whitespace]);
        }
        tokens.extend_from_slice(part);
    }
    tokens
}

enum SelectorItem {
    Compound(Compound),
    Combinator(Combinator),
}

fn parse_complex(tokens: &[Token], nesting: usize, nest: Option<&Nest>) -> ComplexSelector {
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
            nest,
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
    let weight = compounds
        .iter()
        .flat_map(|compound| compound.parts.iter())
        .map(|part| match part {
            Simple::Pseudo(PseudoClass::Is(list)) => list_weight(list),
            Simple::Pseudo(PseudoClass::Not(list)) => list_weight(list),
            _ => 0,
        })
        .fold(compounds.len() as u32, u32::saturating_add);
    if compounds.is_empty() || compounds.len() > MAX_COMPOUNDS || weight > MAX_SELECTOR_WEIGHT {
        let mut undecided = ComplexSelector::undecided();
        undecided.pseudo_element = pseudo_element;
        return undecided;
    }
    ComplexSelector {
        compounds,
        combinators,
        specificity,
        pseudo_element,
        weight,
    }
}

/// The compound selectors of a selector list, those nested in them counted.
fn list_weight(list: &[ComplexSelector]) -> u32 {
    list.iter()
        .map(|selector| selector.weight)
        .fold(0, u32::saturating_add)
}

/// Parse the simple selector at `index` into `compound`; return the index
/// past it.
fn parse_simple(
    tokens: &[Token],
    index: usize,
    nesting: usize,
    nest: Option<&Nest>,
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
                        let pseudo = match nest {
                            Some(nest) if nest.scope && name == "scope" => {
                                PseudoClass::Is(nest.parent.clone())
                            }
                            _ => pseudo_class(&name),
                        };
                        compound.parts.push(Simple::Pseudo(pseudo));
                    }
                    index + 2
                }
                Some(Token::Function(name)) => {
                    let (arguments, end) = block_at(tokens, index + 1);
                    let pseudo =
                        functional_pseudo_class(name, arguments, nesting, nest, specificity);
                    compound.parts.push(Simple::Pseudo(pseudo));
                    end
                }
                _ => {
                    compound.parts.push(Simple::Unknown);
                    index + 1
                }
            }
        }
        // `&`: in a nested rule, what the rule it is nested in matches (see
        // [`Nest`]), however deep, as the weight a selector may carry bounds
        // what it stands for; in a rule of its own, the root.
        Token::Delim('&') => {
            let pseudo = match nest {
                Some(nest) => {
                    add_specificity(specificity, nest.specificity);
                    PseudoClass::Is(nest.parent.clone())
                }
                None => PseudoClass::Root,
            };
            compound.parts.push(Simple::Pseudo(pseudo));
            index + 1
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
    nest: Option<&Nest>,
    specificity: &mut (u32, u32, u32),
) -> PseudoClass {
    let name = name.to_ascii_lowercase();
    let list = |specificity: &mut (u32, u32, u32), counts: bool| {
        let list = parse_selector_list(arguments, nesting + 1, nest);
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
        "is" | "matches" | "-webkit-any" | "-moz-any" => {
            PseudoClass::Is(list(specificity, true).into())
        }
        "where" => PseudoClass::Is(list(specificity, false).into()),
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
    /// Its element name, and its ids and classes, as the keys rules are
    /// filtered by (see [`selector_key`]).
    name_key: u64,
    other_keys: Box<[u64]>,
    /// The declarations of its inline style the check reads, each with
    /// whether its `style` attribute is prefixed, parsed when first needed
    /// (see [`Element::inline_style`]).
    inline: std::cell::OnceCell<Box<[(bool, Declaration)]>>,
    /// What the custom properties looked up at it say there, by name (see
    /// [`Cascade::custom_value`]).
    customs: std::cell::RefCell<HashMap<u64, Option<Tri>>>,
    /// Whether it may be a size container, whose size `@container` rules
    /// query, as its style says once the walk has read it.
    container: std::cell::Cell<Tri>,
}

impl Element {
    pub(super) fn new(
        local: &str,
        attributes: Vec<(String, bool, String)>,
        position: Position,
    ) -> Self {
        let lower = local.to_ascii_lowercase();
        let mut element = Element {
            local: local.to_string(),
            name_key: selector_key(KEY_TYPE, &lower),
            lower,
            attributes,
            position,
            other_keys: Box::default(),
            inline: std::cell::OnceCell::new(),
            customs: std::cell::RefCell::default(),
            container: std::cell::Cell::new(Tri::No),
        };
        let classes = || {
            element
                .values("class")
                .flat_map(|(_, classes)| classes.split_ascii_whitespace())
        };
        let mut other_keys = Vec::with_capacity(element.values("id").count() + classes().count());
        other_keys.extend(element.values("id").map(|(_, id)| selector_key(KEY_ID, id)));
        other_keys.extend(classes().map(|class| selector_key(KEY_CLASS, class)));
        element.other_keys = other_keys.into_boxed_slice();
        element
    }

    /// Its keys (see [`selector_key`]).
    fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        std::iter::once(self.name_key).chain(self.other_keys.iter().copied())
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

    /// The declarations of its inline style (see [`Element::inline`]),
    /// parsed the first time they are needed, a unit of work for each token.
    fn inline_style(&self, work: &mut u64) -> Result<&[(bool, Declaration)], DocumentError> {
        if let Some(parsed) = self.inline.get() {
            return Ok(parsed);
        }
        let mut parsed = Vec::new();
        for (prefixed, style) in self.values("style") {
            let tokens = tokenize(style)?;
            *work += tokens.len() as u64;
            parsed.extend(
                split_top_level(&tokens, &Token::Semicolon)
                    .into_iter()
                    .flat_map(|tokens| parse_declarations(tokens).into_iter().flatten())
                    .map(|declaration| (prefixed, declaration)),
            );
        }
        Ok(self.inline.get_or_init(|| parsed.into_boxed_slice()))
    }

    /// What a custom property says at it, where looked up before: `Some`
    /// of what [`Cascade::custom_value`] found.
    fn custom(&self, name: u64) -> Option<Option<Tri>> {
        self.customs.borrow().get(&name).copied()
    }

    fn note_custom(&self, name: u64, value: Option<Tri>) {
        self.customs.borrow_mut().insert(name, value);
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

/// [`attribute_test`] without regard to case, as `to_lowercase` folds it:
/// ASCII values, as nearly all are, compared in place.
fn attribute_test_folded(operator: AttributeOperator, actual: &str, wanted: &str) -> bool {
    if !actual.is_ascii() || !wanted.is_ascii() {
        return attribute_test(operator, &actual.to_lowercase(), &wanted.to_lowercase());
    }
    let (actual, wanted) = (actual.as_bytes(), wanted.as_bytes());
    let starts = |text: &[u8]| {
        text.len() >= wanted.len() && text[..wanted.len()].eq_ignore_ascii_case(wanted)
    };
    match operator {
        AttributeOperator::Exists => true,
        AttributeOperator::Equals => actual.eq_ignore_ascii_case(wanted),
        AttributeOperator::Includes => {
            !wanted.is_empty()
                && !wanted.iter().any(|byte| char::from(*byte).is_whitespace())
                && actual
                    .split(u8::is_ascii_whitespace)
                    .any(|word| word.eq_ignore_ascii_case(wanted))
        }
        AttributeOperator::DashMatch => {
            actual.eq_ignore_ascii_case(wanted)
                || (starts(actual) && actual.get(wanted.len()) == Some(&b'-'))
        }
        AttributeOperator::Prefix => !wanted.is_empty() && starts(actual),
        AttributeOperator::Suffix => {
            !wanted.is_empty()
                && actual.len() >= wanted.len()
                && actual[actual.len() - wanted.len()..].eq_ignore_ascii_case(wanted)
        }
        AttributeOperator::Substring => {
            !wanted.is_empty()
                && actual
                    .windows(wanted.len())
                    .any(|window| window.eq_ignore_ascii_case(wanted))
        }
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
    /// The `~` steps a later sibling may still settle, by the key their
    /// compound requires (`None` for those requiring none): listed when a
    /// sibling first carries the key, and left as each is settled.
    open_steps: HashMap<Option<u64>, Vec<u32>>,
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

    /// Whether an element around a node may be a size container, which an
    /// `@container` rule's query asks of.
    fn in_container(&self, node: Node) -> bool {
        self.stack[..node.depth]
            .iter()
            .any(|element| element.container.get() != Tri::No)
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
                        attribute_test_folded(*operator, actual, value)
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
            // Past the work allowed, a list is not walked.
            PseudoClass::Not(_) | PseudoClass::Is(_) if *work > MAX_MATCH_WORK => Tri::Maybe,
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

/// How surely a rule applies to the element at `node`: as its selector
/// matches there, capped by the conditions around it (`condition`); a rule
/// in `@container` only where an element around it may be a size
/// container.
fn rule_applies(
    rule: &StyleRule,
    condition: Applies,
    recorded: &[Option<u32>],
    tree: &Tree,
    node: Node,
    work: &mut u64,
) -> Applies {
    if condition == Applies::No || rule.container && !tree.in_container(node) {
        return Applies::No;
    }
    Applies::matched(match_complex_at(&rule.selector, recorded, tree, node, work)).min(condition)
}

// ---------------------------------------------------------------------------
// Stylesheets

/// A style rule that sets a property the check reads.
#[derive(Debug)]
struct StyleRule {
    selector: ComplexSelector,
    declarations: Rc<[Declaration]>,
    /// Its painting declarations (see [`Cascade::references`]).
    paintings: Rc<[Painting]>,
    /// The classes, ids, and element names its ancestor compounds require
    /// (see [`AncestorKeys`]).
    ancestor_keys: Box<[u64]>,
    /// How the conditions of the at-rules around it hold, which caps how
    /// surely it applies (see [`RuleContext`]).
    condition: Applies,
    /// It stands in an `@container` rule, which applies only inside a size
    /// container.
    container: bool,
    /// It stands in an `@scope` rule, which beats an unscoped rule as
    /// specific as it (scope proximity).
    scoped: bool,
    /// The cascade layer it belongs to, in its sheet's
    /// [`Stylesheet::layers`].
    layer: Option<u32>,
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

/// Where a key sets its bit in a filter of 256 bits: by its lowest byte.
fn key_bit(key: u64) -> (usize, u64) {
    (usize::from(key as u8 >> 6), 1 << (key & 63))
}

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
/// key, as browsers filter rules by their ancestors; and those counted by
/// the lowest byte of their keys, which sets most such rules aside without
/// one.
pub(super) struct AncestorKeys {
    counts: HashMap<u64, u32>,
    bytes: [u32; 256],
}

impl Default for AncestorKeys {
    fn default() -> Self {
        AncestorKeys {
            counts: HashMap::new(),
            bytes: [0; 256],
        }
    }
}

impl AncestorKeys {
    fn keys(element: &Element) -> impl Iterator<Item = u64> + '_ {
        element.keys()
    }

    fn push(&mut self, element: &Element) {
        for key in Self::keys(element) {
            self.bytes[usize::from(key as u8)] += 1;
            *self.counts.entry(key).or_default() += 1;
        }
    }

    fn pop(&mut self, element: &Element) {
        for key in Self::keys(element) {
            if let Some(count) = self.counts.get_mut(&key) {
                self.bytes[usize::from(key as u8)] -= 1;
                *count -= 1;
                if *count == 0 {
                    self.counts.remove(&key);
                }
            }
        }
    }

    /// Whether some open element carries each key.
    fn hold(&self, keys: &[u64]) -> bool {
        keys.iter()
            .all(|key| self.bytes[usize::from(*key as u8)] > 0 && self.counts.contains_key(key))
    }
}

/// A stylesheet reduced to what the check reads: its rules that set
/// `display`, `visibility`, or `content-visibility` where their conditions
/// may hold, and the stylesheets it imports where their conditions may
/// hold, with how they hold, in order. Rules that set a margin, padding,
/// width, or flex basis, or that name an SVG resource to paint, are kept
/// apart as well, and counted where they set nothing else.
#[derive(Debug, Default)]
pub(super) struct Stylesheet {
    rules: Vec<Rc<StyleRule>>,
    spacing: Vec<Rc<StyleRule>>,
    painting: Vec<Rc<StyleRule>>,
    customs: Vec<Rc<StyleRule>>,
    /// The rules kept only for their spacing, painting, or custom
    /// properties.
    others: usize,
    pub(super) imports: Vec<(String, Applies)>,
    /// The cascade layers it declares, in order, each by its names from the
    /// outermost; an anonymous one by a name no sheet can write.
    layers: Vec<Box<[String]>>,
}

impl Stylesheet {
    pub(super) fn rule_count(&self) -> usize {
        self.rules.len() + self.others
    }

    /// The layer named `names` inside `parent` (an anonymous one where
    /// `names` is `None`), declared where it first comes.
    fn layer(&mut self, parent: Option<u32>, names: Option<Vec<String>>) -> u32 {
        let mut path: Vec<String> =
            parent.map_or_else(Vec::new, |parent| self.layers[parent as usize].to_vec());
        match names {
            Some(names) => path.extend(names),
            None => path.push(format!(" {}", self.layers.len())),
        }
        if let Some(known) = self.layers.iter().position(|layer| **layer == *path) {
            return known as u32;
        }
        self.layers.push(path.into_boxed_slice());
        self.layers.len() as u32 - 1
    }
}

/// Nesting of at-rules and nested style rules followed before a sheet is
/// refused as a resource limit.
const MAX_RULE_NESTING: usize = 32;
/// Cascade layers one stylesheet may declare.
const MAX_LAYERS_PER_SHEET: usize = 256;

/// What the at-rules around a rule say of it: how their conditions hold on
/// the readers the check follows (`@media`, `@supports`, the size of a
/// container, which is not read, and a scope's limits); whether it stands
/// in `@container` or `@scope`; and the cascade layer it belongs to, as an
/// index into [`Stylesheet::layers`].
#[derive(Clone, Copy)]
struct RuleContext {
    condition: Applies,
    container: bool,
    scoped: bool,
    layer: Option<u32>,
}

/// Parse a stylesheet the way a reading system reads it.
pub(super) fn parse_stylesheet(css: &str) -> Result<Stylesheet, DocumentError> {
    let tokens = tokenize(css)?;
    let mut sheet = Stylesheet::default();
    let context = RuleContext {
        condition: Applies::Yes,
        container: false,
        scoped: false,
        layer: None,
    };
    parse_rule_list(&tokens, true, context, None, &mut sheet, 0)?;
    Ok(sheet)
}

/// Parse a list of rules; `scope` says what `:scope` and `&` stand for in
/// the style rules of an `@scope` rule.
fn parse_rule_list(
    tokens: &[Token],
    top_level: bool,
    context: RuleContext,
    scope: Option<&Nest>,
    sheet: &mut Stylesheet,
    nesting: usize,
) -> Result<(), DocumentError> {
    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index] {
            Token::Whitespace | Token::Cdo | Token::Cdc | Token::Semicolon => index += 1,
            Token::AtKeyword(name) => {
                index = parse_at_rule(
                    tokens, index, name, top_level, context, scope, None, sheet, nesting,
                )?;
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
                let selectors = RuleSelectors::new(&tokens[start..index], scope.cloned());
                parse_style_block(&selectors, block, context, sheet, nesting)?;
                index = end;
            }
        }
    }
    Ok(())
}

/// The names of a cascade layer (`base`, `theme.dark`), or `None` for an
/// anonymous one; `Err` for a prelude that names none.
fn layer_names(prelude: &[Token]) -> Result<Option<Vec<String>>, ()> {
    let prelude = trim_whitespace(prelude);
    if prelude.is_empty() {
        return Ok(None);
    }
    let mut names = Vec::new();
    for (at, token) in prelude.iter().enumerate() {
        match (at % 2, token) {
            (0, Token::Ident(name)) => names.push(name.clone()),
            (1, Token::Delim('.')) => {}
            _ => return Err(()),
        }
    }
    if prelude.len().is_multiple_of(2) {
        return Err(());
    }
    Ok(Some(names))
}

/// Parse the at-rule at `index`; return the index past it. `selectors` are
/// the enclosing style rule's for an at-rule nested in one, whose
/// declarations style the same elements, and whose nested rules are nested
/// in that rule.
#[allow(clippy::too_many_arguments)]
fn parse_at_rule(
    tokens: &[Token],
    index: usize,
    name: &str,
    top_level: bool,
    context: RuleContext,
    scope: Option<&Nest>,
    selectors: Option<&RuleSelectors>,
    sheet: &mut Stylesheet,
    nesting: usize,
) -> Result<usize, DocumentError> {
    let name = name.to_ascii_lowercase();
    let mut end = index + 1;
    while end < tokens.len() && !matches!(tokens[end], Token::Semicolon | Token::OpenCurly) {
        end = skip_component(tokens, end);
    }
    let prelude = &tokens[index + 1..end];
    let within = |condition: Applies| RuleContext {
        condition: context.condition.min(condition),
        ..context
    };
    if end < tokens.len() && tokens[end] == Token::OpenCurly {
        let (block, after) = block_at(tokens, end);
        let mut scope = scope.cloned();
        let inner = match name.as_str() {
            "media" => Some(within(media_condition(prelude))),
            "supports" => Some(within(supports_condition(prelude))),
            // A container's size is not read: its rules may apply inside a
            // size container (see [`Cascade::rule_applies`]).
            "container" => Some(RuleContext {
                container: true,
                ..within(Applies::Doubt)
            }),
            "scope" => match (selectors, scope_prelude(prelude)) {
                (None, Some((root, limited))) => {
                    scope = Some(root);
                    let limit = if limited {
                        Applies::Doubt
                    } else {
                        Applies::Yes
                    };
                    Some(RuleContext {
                        scoped: true,
                        ..within(limit)
                    })
                }
                // A scope whose root is the parent of the sheet's owner, or
                // nested in a style rule, may apply.
                (_, _) => Some(within(Applies::Doubt)),
            },
            "layer" => match layer_names(prelude) {
                Ok(names) => {
                    if sheet.layers.len() >= MAX_LAYERS_PER_SHEET {
                        return Err(DocumentError::ResourceLimit);
                    }
                    Some(RuleContext {
                        layer: Some(sheet.layer(context.layer, names)),
                        ..context
                    })
                }
                Err(()) => None,
            },
            // Starting styles apply only as a transition begins, not to
            // what a reader then shows; no reader this check follows
            // applies `@document` rules. `@font-face`, `@page`,
            // `@keyframes`, and unknown at-rules style no elements.
            _ => None,
        };
        if let Some(inner) = inner.filter(|inner| inner.condition != Applies::No) {
            if nesting >= MAX_RULE_NESTING {
                return Err(DocumentError::ResourceLimit);
            }
            match selectors {
                Some(selectors) => {
                    parse_style_block(selectors, block, inner, sheet, nesting + 1)?;
                }
                None => parse_rule_list(block, false, inner, scope.as_ref(), sheet, nesting + 1)?,
            }
        }
        return Ok(after);
    }
    if name == "layer" && context.condition != Applies::No {
        // `@layer a, b;` declares the layers, in that order.
        for names in split_top_level(prelude, &Token::Comma) {
            if let Ok(Some(names)) = layer_names(names) {
                if sheet.layers.len() >= MAX_LAYERS_PER_SHEET {
                    return Err(DocumentError::ResourceLimit);
                }
                sheet.layer(context.layer, Some(names));
            }
        }
    }
    if name == "import" && top_level && selectors.is_none() {
        if let Some((target, applies)) = import_target(prelude) {
            let applies = applies.min(context.condition);
            if applies != Applies::No {
                if sheet.imports.len() >= MAX_IMPORTS_PER_SHEET {
                    return Err(DocumentError::ResourceLimit);
                }
                sheet.imports.push((target, applies));
            }
        }
    }
    Ok((end + 1).min(tokens.len()))
}

/// The root an `@scope` rule's style rules match inside, as what `&` and
/// `:scope` stand for in them, and whether limits (`to (...)`) cut the
/// scope short; `None` for a rule without a root.
fn scope_prelude(prelude: &[Token]) -> Option<(Nest, bool)> {
    let parts = components(trim_whitespace(prelude));
    let parenthesized = |part: &[Token]| part.first() == Some(&Token::OpenParen);
    let (root, limited) = match parts.as_slice() {
        [root] if parenthesized(root) => (root, false),
        [root, to, limit] if parenthesized(root) && is_word(to, "to") && parenthesized(limit) => {
            (root, true)
        }
        _ => return None,
    };
    let list = parse_selector_list(trim_whitespace(block_contents(root)), 0, None);
    let nest = Nest {
        parent: list
            .into_iter()
            .filter(|selector| selector.pseudo_element == PseudoElement::None)
            .collect(),
        specificity: (0, 0, 0),
        scope: true,
    };
    Some((nest, limited))
}

/// An `@import`'s target, and how its `supports()` condition and media list
/// hold on the readers the check follows.
fn import_target(prelude: &[Token]) -> Option<(String, Applies)> {
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
    let mut supported = Applies::Yes;
    loop {
        match rest {
            [Token::Ident(word), tail @ ..] if word.eq_ignore_ascii_case("layer") => {
                rest = trim_whitespace(tail);
            }
            [Token::Function(function), ..]
                if function.eq_ignore_ascii_case("layer")
                    || function.eq_ignore_ascii_case("supports") =>
            {
                let (arguments, end) = block_at(rest, 0);
                if function.eq_ignore_ascii_case("supports") {
                    // A declaration, or a condition in its own right.
                    let arguments = trim_whitespace(arguments);
                    supported = match arguments {
                        [Token::Ident(name), tail @ ..] => match trim_whitespace(tail) {
                            [Token::Colon, value @ ..] => supports_declaration(name, value),
                            _ => supports_condition(arguments),
                        },
                        _ => supports_condition(arguments),
                    };
                }
                rest = trim_whitespace(&rest[end..]);
            }
            _ => break,
        }
    }
    Some((target, supported.min(media_condition(rest))))
}

/// A style rule's selectors, read once a declaration or a nested rule
/// needs them; for a rule nested in another, with what `&` stands for.
struct RuleSelectors<'a> {
    prelude: &'a [Token],
    parent: Option<Nest>,
    list: std::cell::OnceCell<Vec<ComplexSelector>>,
    nest: std::cell::OnceCell<Nest>,
}

impl<'a> RuleSelectors<'a> {
    fn new(prelude: &'a [Token], parent: Option<Nest>) -> Self {
        RuleSelectors {
            prelude,
            parent,
            list: std::cell::OnceCell::new(),
            nest: std::cell::OnceCell::new(),
        }
    }

    fn list(&self) -> &[ComplexSelector] {
        self.list.get_or_init(|| match &self.parent {
            Some(parent) => parse_selector_list(
                &nested_selectors(self.prelude, parent.scope),
                0,
                Some(parent),
            ),
            None => parse_selector_list(self.prelude, 0, None),
        })
    }

    /// What `&` stands for in the rules nested in this one.
    fn nest(&self) -> Nest {
        self.nest
            .get_or_init(|| {
                let list = self.list();
                Nest {
                    parent: list
                        .iter()
                        .filter(|selector| selector.pseudo_element == PseudoElement::None)
                        .cloned()
                        .collect(),
                    specificity: list
                        .iter()
                        .map(|selector| selector.specificity)
                        .max()
                        .unwrap_or((0, 0, 0)),
                    scope: false,
                }
            })
            .clone()
    }
}

/// A style rule's block: its declarations, and the rules nested in it,
/// whose selectors are read inside this rule's (see [`Nest`]). As a reader
/// orders them, the declarations before a nested rule, or a nested
/// at-rule, come before it in the cascade, and those after it after it.
fn parse_style_block(
    selectors: &RuleSelectors,
    block: &[Token],
    context: RuleContext,
    sheet: &mut Stylesheet,
    nesting: usize,
) -> Result<(), DocumentError> {
    let mut declarations = Vec::new();
    let mut paintings = Vec::new();
    let mut index = 0;
    while index < block.len() {
        match &block[index] {
            Token::Whitespace | Token::Semicolon => index += 1,
            Token::AtKeyword(name) => {
                push_style_rule(selectors, &mut declarations, &mut paintings, context, sheet)?;
                index = parse_at_rule(
                    block,
                    index,
                    name,
                    false,
                    context,
                    None,
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
                    push_style_rule(selectors, &mut declarations, &mut paintings, context, sheet)?;
                    let (inner, end) = block_at(block, index);
                    let nested = RuleSelectors::new(&block[start..index], Some(selectors.nest()));
                    parse_style_block(&nested, inner, context, sheet, nesting + 1)?;
                    index = end;
                } else {
                    declarations.extend(
                        parse_declarations(&block[start..index])
                            .into_iter()
                            .flatten(),
                    );
                    paintings.extend(parse_painting(&block[start..index]));
                    index += 1;
                }
            }
        }
    }
    push_style_rule(selectors, &mut declarations, &mut paintings, context, sheet)
}

/// Keep the declarations read so far of a style rule's block as a rule of
/// their own, where they set what the check reads, and start afresh.
fn push_style_rule(
    selectors: &RuleSelectors,
    declarations: &mut Vec<Declaration>,
    paintings: &mut Vec<Painting>,
    context: RuleContext,
    sheet: &mut Stylesheet,
) -> Result<(), DocumentError> {
    let declarations = std::mem::take(declarations);
    let paintings = std::mem::take(paintings);
    if context.condition != Applies::No && !(declarations.is_empty() && paintings.is_empty()) {
        let declarations: Rc<[Declaration]> = declarations.into();
        let paintings: Rc<[Painting]> = paintings.into();
        // A `::before` or `::after` box matters for where a reader breaks
        // lines, which its `display`, `content`, `float`, `position`, and
        // `white-space` decide, for the text it shows, which its
        // `visibility` and `opacity` may keep unseen, and for whether its
        // margins and padding set a dash apart from the element's content.
        // An element's own `content` and `white-space` are not read, its
        // `opacity` only where it refers to an SVG resource, and its
        // margins, padding, width, and flex basis only for flex and grid
        // items (see [`Cascade::item_box`]). Painting properties that name
        // an SVG resource are kept apart (see [`Cascade::references`]).
        let lays_out = declarations.iter().any(|declaration| {
            matches!(
                declaration.property,
                Property::Display
                    | Property::Content
                    | Property::Float
                    | Property::Position
                    | Property::WhiteSpace
                    | Property::Visibility
                    | Property::Opacity
            )
        });
        let styles_element = declarations.iter().any(|declaration| {
            !declaration.property.spaces()
                && !matches!(
                    declaration.property,
                    Property::Content | Property::WhiteSpace | Property::Custom
                )
        });
        let customizes = declarations
            .iter()
            .any(|declaration| declaration.property == Property::Custom);
        let spaces = declarations
            .iter()
            .any(|declaration| declaration.property.spaces());
        for selector in selectors.list() {
            let kept = match selector.pseudo_element {
                PseudoElement::None => styles_element,
                PseudoElement::Before | PseudoElement::After => lays_out || spaces,
                PseudoElement::Other => false,
            };
            let spacing = spaces && selector.pseudo_element == PseudoElement::None;
            let painting = !paintings.is_empty() && selector.pseudo_element == PseudoElement::None;
            let custom = customizes && selector.pseudo_element == PseudoElement::None;
            if kept || spacing || painting || custom {
                let ancestor_keys = ancestor_keys(selector, selector.compounds.len() - 1);
                let rule = Rc::new(StyleRule {
                    selector: selector.clone(),
                    declarations: declarations.clone(),
                    paintings: paintings.clone(),
                    ancestor_keys,
                    condition: context.condition,
                    container: context.container,
                    scoped: context.scoped,
                    layer: context.layer,
                });
                if painting {
                    sheet.painting.push(rule.clone());
                }
                if custom {
                    sheet.customs.push(rule.clone());
                }
                if spacing {
                    sheet.spacing.push(rule.clone());
                }
                sheet.others += usize::from(!kept);
                if kept {
                    sheet.rules.push(rule);
                }
            }
        }
    }
    if sheet.rule_count() > MAX_STYLE_RULES {
        return Err(DocumentError::ResourceLimit);
    }
    Ok(())
}

/// Where a painting declaration stands in the cascade, how surely it
/// applies, and the SVG resource it names, if one (see
/// [`Cascade::references`]).
type Named = (Precedence, Tri, Option<Rc<str>>);

/// A declaration of a painting property (see [`PAINTING_PROPERTIES`]): which
/// one, whether it is `!important`, and the SVG resource its value names
/// (`url(#id)`), `None` for any other value, which names none.
#[derive(Clone, Debug)]
struct Painting {
    property: u8,
    important: bool,
    target: Option<Rc<str>>,
}

/// The painting declaration a token run holds, if it holds one.
fn parse_painting(tokens: &[Token]) -> Option<Painting> {
    let [Token::Ident(name), rest @ ..] = trim_whitespace(tokens) else {
        return None;
    };
    let property = PAINTING_PROPERTIES
        .iter()
        .position(|property| name.eq_ignore_ascii_case(property))? as u8;
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
    Some(Painting {
        property,
        important,
        target: url_target(value).map(Rc::from),
    })
}

/// The id the first `url(#id)` reference among a value's tokens names: a
/// `url` token, or a `url()` function holding a string.
fn url_target(value: &[Token]) -> Option<&str> {
    let mut index = 0;
    while index < value.len() {
        match &value[index] {
            Token::Url(target) => return fragment(target),
            Token::Function(function) if function.eq_ignore_ascii_case("url") => {
                let (arguments, _) = block_at(value, index);
                return match trim_whitespace(arguments) {
                    [Token::Str(target)] => fragment(target),
                    _ => None,
                };
            }
            _ => {}
        }
        index = skip_component(value, index);
    }
    None
}

// ---------------------------------------------------------------------------
// The reader's cascade

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Precedence {
    tier: u8,
    /// An author rule's cascade layer, by its place (see
    /// [`Cascade::layer_order`]); 0 for presentational hints, inline
    /// styles, and the user agent's rules.
    layer: u32,
    specificity: (u32, u32, u32),
    /// 1 for a rule in an `@scope` rule, which beats an unscoped one as
    /// specific as it; how near its scope's root is to the element is not
    /// read.
    proximity: u8,
    order: u32,
}

const TIER_USER_AGENT: u8 = 0;
const TIER_AUTHOR: u8 = 1;
const TIER_INLINE: u8 = 2;
const TIER_AUTHOR_IMPORTANT: u8 = 3;
const TIER_INLINE_IMPORTANT: u8 = 4;

/// Where a declaration of an inline style stands in the cascade.
fn inline_precedence(important: bool) -> Precedence {
    Precedence {
        tier: if important {
            TIER_INLINE_IMPORTANT
        } else {
            TIER_INLINE
        },
        layer: 0,
        specificity: (0, 0, 0),
        proximity: 0,
        order: 0,
    }
}

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
    /// Whether it is transparent (`opacity: 0`), with all it holds, which
    /// is read only for an SVG element that refers to a resource.
    transparent: Tri,
    /// Whether it is a size container (see [`Element::container`]).
    container: Tri,
    /// Whether it lays its children out as flex or grid items, and how
    /// they stand.
    items: Tri,
    item_layout: ItemLayout,
    /// Whether it certainly has no box, its children taking its place
    /// (`display: contents`).
    contents: bool,
    /// Its `::before` and `::after` boxes.
    before: PseudoBox,
    after: PseudoBox,
}

/// How a box lays its children out as flex or grid items, as its style
/// says: each `Yes` where it certainly does, `Maybe` where it may.
#[derive(Clone, Copy, Debug, Default)]
struct ItemLayout {
    /// The items stand in a column or in reverse, or in a grid's tracks,
    /// which stretch across a block and stack in an inline box without
    /// columns: apart from each other, and the last not beside what
    /// follows an inline box.
    turned: Tri,
    /// A gap, or `justify-content` spreading them along a block's line,
    /// stands between them.
    spaced: Tri,
    /// They may wrap onto more lines.
    wraps: Tri,
}

/// How an item's own box stands among the items beside it (see
/// [`Cascade::item_box`]): whether its left and its right margin or
/// padding set it apart, and whether its width or flex basis takes a
/// whole line, so that where the items wrap, it stands on a line of its
/// own.
#[derive(Clone, Copy, Debug, Default)]
struct ItemBox {
    left: Tri,
    right: Tri,
    full: Tri,
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
    /// What it certainly shows that matters beside a digit: a sign an
    /// amount reads by, or anything that keeps two numbers apart.
    sign: Option<Sign>,
    /// For a `::before` box, whether that sign is hyphens or dashes set
    /// apart from the element's content, as a bullet stands.
    bullet: bool,
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
fn resolve_value<T: Clone + PartialEq>(applied: &[(Precedence, Tri, T)], default: T) -> (T, bool) {
    let best = applied
        .iter()
        .filter(|(_, certainty, _)| *certainty == Tri::Yes)
        .max_by_key(|(precedence, _, _)| *precedence);
    let value = best.map_or(default, |best| best.2.clone());
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
    /// requires, and those that require none; and a bit for the lowest byte
    /// of each key, which sets most siblings aside without a lookup.
    steps_by_key: HashMap<u64, Vec<u32>>,
    steps_anywhere: Vec<u32>,
    step_key_bits: [u64; 4],
    /// Rules that set a margin, padding, width, or flex basis, each with its
    /// place among them and its layer, by an id, class, or element name
    /// their rightmost compound requires, and those that require none (see
    /// [`Cascade::item_box`]); and how many of them set nothing else.
    spacing_rules: Vec<Ranked>,
    spacing_by_key: HashMap<u64, Vec<usize>>,
    spacing_anywhere: Vec<usize>,
    /// Rules that set a painting property, indexed as those setting a
    /// margin or padding are (see [`Cascade::references`]), and the ids
    /// they name.
    painting_rules: Vec<Ranked>,
    painting_by_key: HashMap<u64, Vec<usize>>,
    painting_anywhere: Vec<usize>,
    painted_ids: std::collections::HashSet<Rc<str>>,
    /// Rules that set a custom property, by its name (see
    /// [`Cascade::custom_value`]).
    custom_rules: HashMap<u64, CustomRules>,
    custom_order: u32,
    /// The rules kept only for their spacing, painting, or custom
    /// properties.
    others: usize,
    /// The cascade layers of its sheets, by the names and sublayers of
    /// each, the unlayered rules' first; and each layer's place, found
    /// once all the sheets are in (see [`Cascade::layer_order`]).
    layer_names: Vec<String>,
    layer_children: Vec<Vec<u32>>,
    layer_ranks: std::cell::OnceCell<Box<[u32]>>,
}

/// A rule in a chapter's cascade: its place in the cascade order, its
/// cascade layer, how the conditions around it hold, and the ids of its `~`
/// steps by combinator.
struct CascadeRule {
    rule: Rc<StyleRule>,
    order: u32,
    layer: u32,
    condition: Applies,
    recorded: Box<[Option<u32>]>,
}

/// A rule in a chapter's cascade kept for its spacing, painting, or custom
/// properties: its place among the rules of its kind, its cascade layer,
/// and how the conditions around it hold, those of the link, `style`
/// element, or import that applies its sheet among them.
struct Ranked {
    rule: Rc<StyleRule>,
    order: u32,
    layer: u32,
    condition: Applies,
}

/// The rules that set one custom property, indexed by an id, class, or
/// element name their rightmost compound requires, and those that require
/// none.
#[derive(Default)]
struct CustomRules {
    rules: Vec<Ranked>,
    by_key: HashMap<u64, Vec<u32>>,
    anywhere: Vec<u32>,
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
    /// Take in a sheet's rules, their conditions capped by `condition`, how
    /// the link, `style` element, or import applying it holds.
    pub(super) fn push_sheet(&mut self, sheet: &Stylesheet, condition: Applies) {
        let layers = self.push_layers(sheet);
        let layer = |rule: &StyleRule| rule.layer.map_or(0, |layer| layers[layer as usize]);
        let ranked = |rule: &Rc<StyleRule>, order: u32| Ranked {
            rule: rule.clone(),
            order,
            layer: layer(rule),
            condition: rule.condition.min(condition),
        };
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
                            Some(key) => {
                                let (word, bit) = key_bit(key);
                                self.step_key_bits[word] |= bit;
                                self.steps_by_key.entry(key).or_default().push(step);
                            }
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
                layer: layer(rule),
                condition: rule.condition.min(condition),
                recorded,
            });
        }
        for rule in &sheet.spacing {
            let index = self.spacing_rules.len();
            match rule.selector.compounds.last().and_then(rarest_key) {
                Some(key) => self.spacing_by_key.entry(key).or_default().push(index),
                None => self.spacing_anywhere.push(index),
            }
            self.spacing_rules.push(ranked(rule, index as u32 + 1));
        }
        for rule in &sheet.customs {
            self.custom_order += 1;
            let mut names: Vec<u64> = rule
                .declarations
                .iter()
                .filter(|declaration| declaration.property == Property::Custom)
                .filter_map(|declaration| declaration.var)
                .collect();
            names.sort_unstable();
            names.dedup();
            let key = rule.selector.compounds.last().and_then(rarest_key);
            for name in names {
                let rules = self.custom_rules.entry(name).or_default();
                let index = rules.rules.len() as u32;
                match key {
                    Some(key) => rules.by_key.entry(key).or_default().push(index),
                    None => rules.anywhere.push(index),
                }
                rules.rules.push(ranked(rule, self.custom_order));
            }
        }
        for rule in &sheet.painting {
            let index = self.painting_rules.len();
            match rule.selector.compounds.last().and_then(rarest_key) {
                Some(key) => self.painting_by_key.entry(key).or_default().push(index),
                None => self.painting_anywhere.push(index),
            }
            self.painted_ids.extend(
                rule.paintings
                    .iter()
                    .filter_map(|painting| painting.target.clone()),
            );
            self.painting_rules.push(ranked(rule, index as u32 + 1));
        }
        self.others += sheet.others;
    }

    /// Take in the cascade layers a sheet declares, in its order: a named
    /// layer is the one of that name inside its parent, wherever declared
    /// before, and an anonymous one is new to this sheet. Their nodes, by
    /// the sheet's layers.
    fn push_layers(&mut self, sheet: &Stylesheet) -> Vec<u32> {
        if self.layer_children.is_empty() {
            self.layer_names.push(String::new());
            self.layer_children.push(Vec::new());
        }
        self.layer_ranks.take();
        let mut anonymous: HashMap<&str, u32> = HashMap::new();
        let mut nodes = Vec::with_capacity(sheet.layers.len());
        for path in &sheet.layers {
            let mut node = 0u32;
            for name in path.iter() {
                let known = if name.starts_with(' ') {
                    anonymous.get(name.as_str()).copied()
                } else {
                    self.layer_children[node as usize]
                        .iter()
                        .copied()
                        .find(|child| self.layer_names[*child as usize] == *name)
                };
                node = match known {
                    Some(child) => child,
                    None => {
                        let child = self.layer_names.len() as u32;
                        self.layer_names.push(name.clone());
                        self.layer_children.push(Vec::new());
                        self.layer_children[node as usize].push(child);
                        if name.starts_with(' ') {
                            anonymous.insert(name, child);
                        }
                        child
                    }
                };
            }
            nodes.push(node);
        }
        nodes
    }

    /// Where a rule's cascade layer places it among the author rules (CSS
    /// Cascade 5): each layer after its sublayers, which come in the order
    /// they were first declared, and the unlayered rules last. A later
    /// place wins for normal declarations; for `!important` ones, an
    /// earlier.
    fn layer_order(&self, layer: u32, important: bool) -> u32 {
        let ranks = self.layer_ranks.get_or_init(|| {
            let mut ranks = vec![0u32; self.layer_children.len().max(1)];
            let mut next = 1;
            let mut stack = vec![(0u32, 0usize)];
            while let Some((node, child)) = stack.pop() {
                match self
                    .layer_children
                    .get(node as usize)
                    .and_then(|children| children.get(child))
                {
                    Some(&first) => {
                        stack.push((node, child + 1));
                        stack.push((first, 0));
                    }
                    None => {
                        ranks[node as usize] = next;
                        next += 1;
                    }
                }
            }
            ranks.into_boxed_slice()
        });
        let rank = ranks.get(layer as usize).copied().unwrap_or(0);
        if important {
            u32::MAX - rank
        } else {
            rank
        }
    }

    /// Where an author rule's declaration stands in the cascade: its tier,
    /// its layer's place, its selector's specificity, and its order.
    fn precedence(
        &self,
        declaration: &Declaration,
        rule: &StyleRule,
        order: u32,
        layer: u32,
    ) -> Precedence {
        self.rule_precedence(declaration.important, rule, order, layer)
    }

    fn rule_precedence(
        &self,
        important: bool,
        rule: &StyleRule,
        order: u32,
        layer: u32,
    ) -> Precedence {
        let tier = if important {
            TIER_AUTHOR_IMPORTANT
        } else {
            TIER_AUTHOR
        };
        Precedence {
            tier,
            layer: self.layer_order(layer, important),
            specificity: rule.selector.specificity,
            proximity: u8::from(rule.scoped),
            order,
        }
    }

    /// A declaration with what a custom property it takes its size from
    /// says at the element at the top of the tree (see [`Declaration::var`]).
    fn resolved(
        &self,
        declaration: &Declaration,
        tree: &Tree,
        ancestors: &AncestorKeys,
        work: &mut u64,
    ) -> Result<Declaration, DocumentError> {
        match declaration.var {
            Some(name) if declaration.property != Property::Custom => {
                let unset = declaration.flow.unwrap_or(Tri::Maybe);
                let value = self.custom_value(tree, ancestors, name, work)?;
                Ok(Declaration {
                    flow: Some(value.unwrap_or(unset)),
                    var: None,
                    ..*declaration
                })
            }
            _ => Ok(*declaration),
        }
    }

    /// What the custom property of this name says at the element at the
    /// top of the tree: set by a rule or its inline style, or inherited
    /// from the nearest element around it that sets it (see
    /// [`Cascade::custom_set`]); `None` where nothing sets it. Each element
    /// keeps what it found, so a property is settled once at each.
    fn custom_value(
        &self,
        tree: &Tree,
        ancestors: &AncestorKeys,
        name: u64,
        work: &mut u64,
    ) -> Result<Option<Tri>, DocumentError> {
        let mut inheriting = Vec::new();
        let mut value = None;
        for depth in (0..tree.stack.len()).rev() {
            let element = &tree.stack[depth];
            if let Some(known) = element.custom(name) {
                value = known;
                break;
            }
            if let Some(set) = self.custom_set(tree, ancestors, depth, name, work)? {
                value = Some(set);
                element.note_custom(name, value);
                break;
            }
            inheriting.push(depth);
        }
        for depth in inheriting {
            tree.stack[depth].note_custom(name, value);
        }
        Ok(value)
    }

    /// What the custom property of this name says where the open element
    /// at `depth` sets it, by a rule or its inline style: `Maybe` where
    /// only a rule that may apply sets it there, or one above the one that
    /// certainly does says otherwise; `None` where nothing sets it there.
    /// The rules tried are those whose rightmost compound the element may
    /// fit, and trying each, and reading its inline style, counts as work.
    fn custom_set(
        &self,
        tree: &Tree,
        ancestors: &AncestorKeys,
        depth: usize,
        name: u64,
        work: &mut u64,
    ) -> Result<Option<Tri>, DocumentError> {
        let element = &tree.stack[depth];
        let node = Node {
            depth,
            sibling: None,
        };
        let mut applied: Vec<(Precedence, Tri, Tri)> = Vec::new();
        if let Some(rules) = self.custom_rules.get(&name) {
            let mut candidates: Vec<u32> = element
                .keys()
                .filter_map(|key| rules.by_key.get(&key))
                .flatten()
                .chain(&rules.anywhere)
                .copied()
                .collect();
            candidates.sort_unstable();
            candidates.dedup();
            for index in candidates {
                let Ranked {
                    rule,
                    order,
                    layer,
                    condition,
                } = &rules.rules[index as usize];
                if !ancestors.hold(&rule.ancestor_keys) {
                    *work += 1;
                    continue;
                }
                let certainty = rule_applies(rule, *condition, &[], tree, node, work).everywhere();
                if certainty == Tri::No {
                    continue;
                }
                for declaration in rule.declarations.iter().filter(|declaration| {
                    declaration.property == Property::Custom && declaration.var == Some(name)
                }) {
                    let precedence = self.precedence(declaration, rule, *order, *layer);
                    let says = declaration.flow.unwrap_or(Tri::Maybe);
                    applied.push((precedence, certainty, says));
                }
            }
        }
        let inline = element.inline_style(work)?;
        *work += inline.len() as u64;
        if *work > MAX_MATCH_WORK {
            return Err(DocumentError::ResourceLimit);
        }
        for (prefixed, declaration) in inline {
            if declaration.property != Property::Custom || declaration.var != Some(name) {
                continue;
            }
            let certainty = if *prefixed { Tri::Maybe } else { Tri::Yes };
            applied.push((
                inline_precedence(declaration.important),
                certainty,
                declaration.flow.unwrap_or(Tri::Maybe),
            ));
        }
        if applied.is_empty() {
            return Ok(None);
        }
        let certain = applied
            .iter()
            .any(|(_, certainty, _)| *certainty == Tri::Yes);
        let (value, contested) = resolve_value(&applied, Tri::Maybe);
        Ok(Some(if certain && !contested {
            value
        } else {
            Tri::Maybe
        }))
    }

    /// Whether a rule may name the SVG resource of this id to paint with,
    /// and whether any rule names one.
    fn may_paint(&self, id: &str) -> bool {
        self.painted_ids.contains(id)
    }

    fn paints_resources(&self) -> bool {
        !self.painting_rules.is_empty()
    }

    /// The ids of the SVG resources the element at the top of the tree
    /// paints with: a pattern as its fill or stroke, a clip path, a mask,
    /// or its markers. For each painting property, the declaration that
    /// certainly wins the cascade among its presentation attributes, the
    /// rules that set the property, and its inline style names the one it
    /// uses; where a declaration that may apply above it says otherwise,
    /// it names none for certain.
    fn references(
        &self,
        tree: &Tree,
        ancestors: &AncestorKeys,
        work: &mut u64,
    ) -> Result<Vec<Rc<str>>, DocumentError> {
        let element = tree.stack.last().expect("an element to style");
        // The fill, the stroke, the clip path, the mask, and the start,
        // middle, and end markers, which `marker` sets together.
        let mut slots: [Vec<Named>; 7] = Default::default();
        let mut add = |painting: &Painting, precedence: Precedence, certainty: Tri| {
            let targets: &[usize] = match painting.property {
                0..=3 => &[painting.property as usize][..],
                4 => &[4, 5, 6],
                property => &[property as usize - 1][..],
            };
            for &slot in targets {
                slots[slot].push((precedence, certainty, painting.target.clone()));
            }
        };
        let presentation = Precedence {
            tier: TIER_AUTHOR,
            layer: 0,
            specificity: (0, 0, 0),
            proximity: 0,
            order: 0,
        };
        for (property, name) in PAINTING_PROPERTIES.iter().enumerate() {
            for (prefixed, value) in element.values(name) {
                let certainty = if prefixed { Tri::Maybe } else { Tri::Yes };
                let tokens = tokenize(value)?;
                let painting = Painting {
                    property: property as u8,
                    important: false,
                    target: url_target(trim_whitespace(&tokens)).map(Rc::from),
                };
                add(&painting, presentation, certainty);
            }
        }
        let mut candidates: Vec<usize> = element
            .keys()
            .filter_map(|key| self.painting_by_key.get(&key))
            .flatten()
            .chain(&self.painting_anywhere)
            .copied()
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        for index in candidates {
            let Ranked {
                rule,
                order,
                layer,
                condition,
            } = &self.painting_rules[index];
            if !ancestors.hold(&rule.ancestor_keys) {
                *work += 1;
                continue;
            }
            let certainty =
                rule_applies(rule, *condition, &[], tree, tree.top(), work).everywhere();
            if certainty == Tri::No {
                continue;
            }
            for painting in rule.paintings.iter() {
                let precedence = self.rule_precedence(painting.important, rule, *order, *layer);
                add(painting, precedence, certainty);
            }
        }
        if *work > MAX_MATCH_WORK {
            return Err(DocumentError::ResourceLimit);
        }
        for (prefixed, style) in element.values("style") {
            let certainty = if prefixed { Tri::Maybe } else { Tri::Yes };
            let tokens = tokenize(style)?;
            for painting in split_top_level(&tokens, &Token::Semicolon)
                .into_iter()
                .filter_map(parse_painting)
            {
                let tier = if painting.important {
                    TIER_INLINE_IMPORTANT
                } else {
                    TIER_INLINE
                };
                let precedence = Precedence {
                    tier,
                    layer: 0,
                    specificity: (0, 0, 0),
                    proximity: 0,
                    order: 0,
                };
                add(&painting, precedence, certainty);
            }
        }
        let mut targets: Vec<Rc<str>> = slots
            .iter()
            .filter_map(|slot| match resolve_value(slot, None) {
                (Some(target), false) => Some(target),
                _ => None,
            })
            .collect();
        targets.sort_unstable();
        targets.dedup();
        Ok(targets)
    }

    pub(super) fn rule_count(&self) -> usize {
        self.rules.len() + self.others
    }

    /// How the box of the element at the top of the tree stands among the
    /// items beside it: read only for flex and grid items, and for an
    /// inline box laying them out, from the rules that set a margin,
    /// padding, width, or flex basis and its inline style.
    fn item_box(
        &self,
        tree: &Tree,
        ancestors: &AncestorKeys,
        work: &mut u64,
    ) -> Result<ItemBox, DocumentError> {
        let element = tree.stack.last().expect("an element to style");
        let mut candidates: Vec<usize> = element
            .keys()
            .filter_map(|key| self.spacing_by_key.get(&key))
            .flatten()
            .chain(&self.spacing_anywhere)
            .copied()
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        // The left margin, the right margin, the left padding, the right
        // padding, and the width; and the flex basis, `None` where it
        // takes the width.
        let mut sides: [Vec<(Precedence, Tri, Tri)>; 5] = Default::default();
        let mut bases: Vec<(Precedence, Tri, Option<Tri>)> = Vec::new();
        let mut add = |declaration: &Declaration, precedence: Precedence, certainty: Tri| {
            let slot = match declaration.property {
                Property::MarginLeft => 0,
                Property::MarginRight => 1,
                Property::PaddingLeft => 2,
                Property::PaddingRight => 3,
                Property::Width => 4,
                Property::FlexBasis => {
                    bases.push((precedence, certainty, declaration.flow));
                    return;
                }
                Property::FlexBasisAuto => {
                    bases.push((precedence, certainty, None));
                    return;
                }
                _ => return,
            };
            if let Some(wide) = declaration.flow {
                sides[slot].push((precedence, certainty, wide));
            }
        };
        for index in candidates {
            let Ranked {
                rule,
                order,
                layer,
                condition,
            } = &self.spacing_rules[index];
            if !ancestors.hold(&rule.ancestor_keys) {
                *work += 1;
                continue;
            }
            let certainty =
                rule_applies(rule, *condition, &[], tree, tree.top(), work).everywhere();
            if certainty == Tri::No {
                continue;
            }
            for declaration in rule.declarations.iter() {
                let precedence = self.precedence(declaration, rule, *order, *layer);
                add(
                    &self.resolved(declaration, tree, ancestors, work)?,
                    precedence,
                    certainty,
                );
            }
        }
        if *work > MAX_MATCH_WORK {
            return Err(DocumentError::ResourceLimit);
        }
        for (prefixed, declaration) in element.inline_style(work)? {
            let certainty = if *prefixed { Tri::Maybe } else { Tri::Yes };
            add(
                &self.resolved(declaration, tree, ancestors, work)?,
                inline_precedence(declaration.important),
                certainty,
            );
        }
        // A reader's own margins and padding (HTML's rendering section): a
        // definition's, a quote's, and a figure's margins, and a list's
        // padding at the start of its lines.
        let defaults = match element.lower.as_str() {
            "dd" => [true, false, false, false, false],
            "blockquote" | "figure" => [true, true, false, false, false],
            "ul" | "ol" | "menu" | "dir" => [false, false, true, false, false],
            _ => [false; 5],
        };
        let side = |slot: usize| resolve_flow(&sides[slot], defaults[slot]);
        // A basis other than `auto` sets the item's size along the line in
        // place of its width.
        let full = match resolve_value(&bases, None) {
            (_, true) => Tri::Maybe,
            (None, false) => side(4),
            (Some(basis), false) => basis,
        };
        Ok(ItemBox {
            left: side(0).max(side(2)),
            right: side(1).max(side(3)),
            full,
        })
    }

    /// Record how the element that has just ended among the siblings at
    /// the top of `earlier` fits each `~` step of the rules, with the part
    /// of the selector before it: its siblings before it, and its
    /// ancestors, are those a later sibling's match would reach through
    /// it. A sibling is tried against the steps whose compound requires no
    /// key it lacks: those requiring none, and those by each of its keys.
    /// Each such group is listed for the siblings of one parent the first
    /// time one of them carries its key, and a step leaves the list once a
    /// sibling certainly fits it, which settles it for all after, or once
    /// the open elements lack the ancestors it requires, which they lack for
    /// every sibling of this parent. So a sibling costs a try of each step
    /// still open, however many rules others settled, and all of it counts
    /// as work: past the bound, the chapter is refused as a resource limit.
    fn note_sibling(
        &self,
        stack: &[Element],
        earlier: &mut [Earlier],
        ancestors: &AncestorKeys,
        work: &mut u64,
    ) -> Result<(), DocumentError> {
        if self.sibling_steps.is_empty() {
            return Ok(());
        }
        let depth = stack.len();
        let mut groups: Vec<Option<u64>> = {
            let siblings = &earlier[depth];
            let element = siblings.get(siblings.len() - 1);
            std::iter::once(None)
                .chain(
                    AncestorKeys::keys(element)
                        .filter(|key| {
                            let (word, bit) = key_bit(*key);
                            self.step_key_bits[word] & bit != 0
                                && self.steps_by_key.contains_key(key)
                        })
                        .map(Some),
                )
                .collect()
        };
        groups.sort_unstable();
        groups.dedup();
        for group in groups {
            let steps = match group {
                None => &self.steps_anywhere,
                Some(key) => &self.steps_by_key[&key],
            };
            if steps.is_empty() {
                continue;
            }
            let mut open = match earlier[depth].open_steps.remove(&group) {
                Some(open) => open,
                None => {
                    *work += steps.len() as u64;
                    steps.clone()
                }
            };
            let mut fits = Vec::new();
            {
                let tree = Tree {
                    stack,
                    earlier: &*earlier,
                };
                let node = Node {
                    depth,
                    sibling: Some(tree.earlier[depth].len() - 1),
                };
                open.retain(|&step| {
                    if *work > MAX_MATCH_WORK {
                        return true;
                    }
                    *work += 1;
                    let sibling_step = &self.sibling_steps[step as usize];
                    if !ancestors.hold(&sibling_step.ancestor_keys) {
                        return false;
                    }
                    let CascadeRule { rule, recorded, .. } =
                        &self.rules[sibling_step.rule as usize];
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
                    fit != Tri::Yes
                });
            }
            if *work > MAX_MATCH_WORK {
                return Err(DocumentError::ResourceLimit);
            }
            let siblings = &mut earlier[depth];
            let place = siblings.count - 1;
            for (step, fit) in fits {
                siblings.note_step(step, fit, place);
            }
            siblings.open_steps.insert(group, open);
        }
        Ok(())
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
        // Whether the box is inline-level, floats, is positioned out of the
        // flow, floats to the start of the line, is transparent, and is a
        // size container; how it
        // lays out its children; and, for flex items, whether they stand in
        // a column or in reverse, may wrap, stand in a column of an old
        // flexible box, are spread along the line, and have a gap between
        // them, and whether an old flexible box clamps its lines.
        let mut flows: [Vec<(Precedence, Tri, Tri)>; 6] = Default::default();
        let mut layouts: Vec<(Precedence, Tri, Layout)> = Vec::new();
        let mut item_flags: [Vec<(Precedence, Tri, Tri)>; 6] = Default::default();
        let mut add = |declaration: &Declaration, precedence: Precedence, certainty: Tri| {
            let slot = match declaration.property {
                Property::Display => 0,
                Property::Visibility => 1,
                Property::ContentVisibility => 2,
                Property::Float | Property::Position => {
                    let flow_slot = match declaration.property {
                        Property::Float => 1,
                        _ => 2,
                    };
                    if let Some(flow) = declaration.flow {
                        flows[flow_slot].push((precedence, certainty, flow));
                    }
                    if let Some(side) = declaration.side {
                        flows[3].push((precedence, certainty, side));
                    }
                    return;
                }
                Property::FlexDirection
                | Property::FlexWrap
                | Property::BoxOrient
                | Property::JustifyContent
                | Property::ColumnGap
                | Property::LineClamp => {
                    let flag = match declaration.property {
                        Property::FlexDirection => 0,
                        Property::FlexWrap => 1,
                        Property::BoxOrient => 2,
                        Property::JustifyContent => 3,
                        Property::ColumnGap => 4,
                        _ => 5,
                    };
                    if let Some(says) = declaration.flow {
                        item_flags[flag].push((precedence, certainty, says));
                    }
                    return;
                }
                Property::Opacity | Property::Container => {
                    let slot = match declaration.property {
                        Property::Opacity => 4,
                        _ => 5,
                    };
                    if let Some(says) = declaration.flow {
                        flows[slot].push((precedence, certainty, says));
                    }
                    return;
                }
                // What only `::before` and `::after` boxes read, and margins,
                // padding, widths, and flex bases, read only for flex and
                // grid items (see [`Cascade::item_box`]).
                Property::Content
                | Property::WhiteSpace
                | Property::Custom
                | Property::Width
                | Property::FlexBasis
                | Property::FlexBasisAuto
                | Property::MarginLeft
                | Property::MarginRight
                | Property::PaddingLeft
                | Property::PaddingRight => return,
            };
            if let (Property::Display, Some(flow)) = (declaration.property, declaration.flow) {
                flows[0].push((precedence, certainty, flow));
            }
            if let (Property::Display, Some(layout)) = (declaration.property, declaration.layout) {
                layouts.push((precedence, certainty, layout));
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
        // known until run time, `None` to take the element's), whether it
        // is transparent, and whether a margin or padding sets it apart
        // from the element's content (its right for `::before`, its left
        // for `::after`).
        let mut pseudo_blocks: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_none: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_content: [Vec<(Precedence, Tri, Option<Generated>)>; 2] = Default::default();
        let mut pseudo_out: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_floats: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_line_feeds: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_hidden: [Vec<(Precedence, Tri, Option<Tri>)>; 2] = Default::default();
        let mut pseudo_clear: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_margin: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        let mut pseudo_padding: [Vec<(Precedence, Tri, Tri)>; 2] = Default::default();
        // Whether a rule for each box may apply at all.
        let mut pseudo_styled = [false; 2];
        for index in candidates {
            let CascadeRule {
                rule,
                order,
                layer,
                condition,
                recorded,
            } = &self.rules[index];
            if !ancestors.hold(&rule.ancestor_keys) {
                *work += 1;
                continue;
            }
            let certainty =
                rule_applies(rule, *condition, recorded, tree, tree.top(), work).everywhere();
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
                pseudo_styled[slot] = true;
                for declaration in rule.declarations.iter() {
                    // A box's margin and padding are read on the side that
                    // faces the element's content.
                    let facing = match declaration.property {
                        Property::MarginRight | Property::PaddingRight => slot == 0,
                        Property::MarginLeft | Property::PaddingLeft => slot == 1,
                        _ => true,
                    };
                    if !facing {
                        continue;
                    }
                    let precedence = self.precedence(declaration, rule, *order, *layer);
                    let declaration = &self.resolved(declaration, tree, ancestors, work)?;
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
                        (Property::MarginRight, Some(wide)) if slot == 0 => {
                            pseudo_margin[slot].push((precedence, certainty, wide));
                            continue;
                        }
                        (Property::MarginLeft, Some(wide)) if slot == 1 => {
                            pseudo_margin[slot].push((precedence, certainty, wide));
                            continue;
                        }
                        (Property::PaddingRight, Some(wide)) if slot == 0 => {
                            pseudo_padding[slot].push((precedence, certainty, wide));
                            continue;
                        }
                        (Property::PaddingLeft, Some(wide)) if slot == 1 => {
                            pseudo_padding[slot].push((precedence, certainty, wide));
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
                // Margins, padding, widths, and flex bases are read for
                // items alone (see [`Cascade::item_box`]).
                if declaration.property.spaces() {
                    continue;
                }
                let precedence = self.precedence(declaration, rule, *order, *layer);
                add(
                    &self.resolved(declaration, tree, ancestors, work)?,
                    precedence,
                    certainty,
                );
            }
        }
        if *work > MAX_MATCH_WORK {
            return Err(DocumentError::ResourceLimit);
        }
        for (prefixed, declaration) in element.inline_style(work)? {
            if declaration.property.spaces() {
                continue;
            }
            let certainty = if *prefixed { Tri::Maybe } else { Tri::Yes };
            add(
                &self.resolved(declaration, tree, ancestors, work)?,
                inline_precedence(declaration.important),
                certainty,
            );
        }
        // SVG presentation attributes: author styles that every rule beats.
        let presentation = Precedence {
            tier: TIER_AUTHOR,
            layer: 0,
            specificity: (0, 0, 0),
            proximity: 0,
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
                    layout: None,
                    side: None,
                    generated: None,
                    var: None,
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
                    layout: None,
                    side: None,
                    generated: None,
                    var: None,
                },
                presentation,
                certainty,
            );
        }
        for (prefixed, value) in element.values("opacity") {
            let certainty = if prefixed { Tri::Maybe } else { Tri::Yes };
            let clear =
                svg_number(value).map(|opacity| if opacity <= 0.0 { Tri::Yes } else { Tri::No });
            add(
                &Declaration {
                    property: Property::Opacity,
                    effect: Effect::Neutral,
                    important: false,
                    flow: clear,
                    layout: None,
                    side: None,
                    generated: None,
                    var: None,
                },
                presentation,
                certainty,
            );
        }
        // User-agent rules (HTML's rendering section) that hide content.
        let user_agent = Precedence {
            tier: TIER_USER_AGENT,
            layer: 0,
            specificity: (0, 0, 0),
            proximity: 0,
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
                    layout: None,
                    side: None,
                    generated: None,
                    var: None,
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
            if !pseudo_styled[slot] {
                return PseudoBox::default();
            }
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
            // Hyphens or dashes set apart from the element's content, by
            // white space, a margin or padding, or out of the flow: before
            // it, a bullet; after it, a separator from what follows.
            // Touching the content, a minus.
            let apart = |spaced: bool| {
                spaced
                    || resolve_flow(&pseudo_margin[slot], false)
                        .max(resolve_flow(&pseudo_padding[slot], false))
                        .max(out_of_flow)
                        == Tri::Yes
            };
            let (sign, bullet) = match content {
                Some(Generated::Sign(Sign::Dash { trailing, .. })) if certain && slot == 0 => {
                    (Some(Sign::Amount), apart(trailing))
                }
                Some(Generated::Sign(Sign::Dash { leading, .. })) if certain && apart(leading) => {
                    (Some(Sign::Between), false)
                }
                Some(Generated::Sign(Sign::Dash { .. })) if certain => (Some(Sign::Amount), false),
                Some(Generated::Sign(sign)) if certain => (Some(sign), false),
                Some(Generated::Ornament | Generated::LineFeed) if certain => {
                    (Some(Sign::Between), false)
                }
                _ => (None, false),
            };
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
                bullet,
                unseen,
            }
        };
        let (before, after) = (pseudo(0), pseudo(1));
        // How the box lays out its children. Flex items stand apart in a
        // column or in reverse, and with a gap or spread along a block's
        // line; in an inline box, which is as wide as they are, nothing
        // spreads them. An old flexible box in a column keeps its inline
        // children in lines as a block does, as Blink and WebKit lay it out,
        // and one that clamps its lines is a block. A grid's tracks stretch
        // across a block, and stack in an inline box without columns: its
        // items stand apart.
        let inline = resolve_flow(&flows[0], !reader_block_by_default(element));
        let flag = |slot: usize| resolve_flow(&item_flags[slot], false);
        let (layout, contested) = resolve_value(&layouts, Layout::Flow);
        let (items, item_layout) = match layout {
            _ if contested => (Tri::Maybe, ItemLayout::default()),
            Layout::Flow | Layout::Contents => (Tri::No, ItemLayout::default()),
            Layout::Unknown => (Tri::Maybe, ItemLayout::default()),
            Layout::Flex => {
                let spread = if inline == Tri::Yes { Tri::No } else { flag(3) };
                let item_layout = ItemLayout {
                    turned: flag(0),
                    spaced: flag(4).max(spread),
                    wraps: flag(1),
                };
                (Tri::Yes, item_layout)
            }
            Layout::Box => (
                all_three(flag(2).not(), flag(5).not(), Tri::Yes),
                ItemLayout::default(),
            ),
            Layout::Grid => {
                let item_layout = ItemLayout {
                    turned: Tri::Yes,
                    spaced: Tri::Yes,
                    wraps: Tri::No,
                };
                (Tri::Yes, item_layout)
            }
        };
        Ok(ReaderStyle {
            display: resolve(&applied[0]),
            visibility: resolve(&applied[1]),
            content_visibility: resolve(&applied[2]),
            inline,
            floats: resolve_flow(&flows[1], false),
            floats_to_start: resolve_flow(&flows[3], false),
            positioned: resolve_flow(&flows[2], false),
            transparent: resolve_flow(&flows[4], false),
            container: resolve_flow(&flows[5], false),
            items,
            item_layout,
            contents: layout == Layout::Contents && !contested,
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
    /// It is, or may be, transparent (`opacity: 0`), with all it holds.
    transparent: bool,
    /// Children sit in an SVG image, outside a `foreignObject`.
    in_svg: bool,
    /// Whether a reader paints what it holds where it stands (see
    /// [`Resources`]).
    paint: Paint,
    /// Inside an SVG `text` element, whose text a reader paints; other SVG
    /// elements paint none right inside them.
    svg_text: bool,
    /// For an SVG `switch`: whether a child it may render has come, which
    /// leaves the later ones unrendered.
    switch_taken: Option<bool>,
    /// The font size an SVG element's attributes or inline style set, on it
    /// or on an element around it inside the image.
    svg_font: Option<f64>,
    /// For an SVG `text` or `tspan` that sets `dx`, how it moves its
    /// glyphs, where it or an element around it moves one apart (see
    /// [`svg_glyph_shifts`]), and how many of them came.
    glyph_shifts: Vec<Tri>,
    shifts_taken: usize,
    /// Children are laid out as flex or grid items, and how they stand so
    /// far.
    items: Tri,
    row: Row,
    /// The element is one of its parent's items.
    item: bool,
    /// It is an inline box laying out items, which stands in the line with
    /// the text around it.
    inline_items: bool,
    /// For an item, or an inline box laying out items: whether its left
    /// and its right margin or padding set it apart from what stands
    /// beside it (see [`Cascade::item_box`]).
    edges: (Tri, Tri),
    /// It has no box (`display: contents`): its children stand among its
    /// parent's items, as its text does.
    passes_row: bool,
    exempt: Exempt,
    /// What the element does to AnyDoc's inline run as it ends.
    effects: Effects,
}

impl Open {
    /// Whether its `dx` list reaches glyphs still to come.
    fn shifts_glyphs(&self) -> bool {
        self.shifts_taken < self.glyph_shifts.len()
    }
}

/// The flex or grid items of a box as the walk goes through them (see
/// [`ItemLayout`]). Each child element in the flow is an item, and so is
/// each run of text straight inside, which a reader wraps in an item of its
/// own.
#[derive(Clone, Copy, Default)]
struct Row {
    layout: ItemLayout,
    /// The items so far.
    count: u32,
    /// Whether the last item's right margin or padding, or the lines of its
    /// own items, set it apart from the next.
    last_right: Tri,
    /// Whether the last item takes a whole line (see [`ItemBox`]).
    last_full: Tri,
    /// Whether the items certainly wrapped onto another line.
    wrapped: Tri,
    /// Text straight inside came last: text after it goes on its item.
    text: bool,
}

impl Row {
    /// Take in the next item, whose box is `item`, and tell whether a
    /// reader sets it apart from the one before: `Yes` where the layout or
    /// the spacing certainly does, or where the items wrap and this item or
    /// the one before takes a whole line; otherwise the two may touch,
    /// which counts where digits meet. The first item starts where the box
    /// does, as far along as its own left edge sets it.
    fn next(&mut self, item: ItemBox) -> Tri {
        let wrapped = self.layout.wraps.min(item.full.max(self.last_full));
        let apart = match self.count {
            0 => item.left,
            _ => {
                self.wrapped = self.wrapped.max(wrapped);
                match self
                    .layout
                    .turned
                    .max(self.layout.spaced)
                    .max(self.last_right)
                    .max(item.left)
                    .max(wrapped)
                {
                    Tri::Yes => Tri::Yes,
                    _ => Tri::Maybe,
                }
            }
        };
        self.count += 1;
        self.text = false;
        self.last_right = Tri::No;
        self.last_full = item.full;
        apart
    }

    /// Take in text straight inside the box: an item of its own, unless it
    /// goes on the text before it.
    fn text(&mut self) -> Tri {
        if self.text {
            return Tri::No;
        }
        let apart = self.next(ItemBox::default());
        self.text = true;
        apart
    }

    /// Whether the last item stands apart from what follows the box: where
    /// the items stand in a column or a grid, or may wrap, the last does not
    /// end the box's first line, which the text after an inline box goes on.
    fn trailing(&self) -> Tri {
        let lines = match (self.count, self.layout.turned, self.layout.wraps) {
            (0 | 1, _, _) => Tri::No,
            (_, Tri::Yes, _) => Tri::Yes,
            (_, Tri::Maybe, _) | (_, _, Tri::Yes | Tri::Maybe) => Tri::Maybe,
            _ => Tri::No,
        };
        lines.max(self.wrapped).max(self.last_right)
    }
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
/// before it, whether it is a float to the line's start that opens its
/// paragraph, and where its inline style places a positioned box.
#[derive(Clone, Copy)]
struct FloatStart {
    glyphs: (u64, u64),
    opens: bool,
    placed: Option<Placement>,
}

/// Where an absolutely positioned box's inline style sets it, in pixels:
/// its top and left edges, and the em of its font, from the same style or
/// the default 16 pixels. Fixed-layout books (InDesign's `_idTextSpan`) set
/// each run of a line so, split wherever the character style or the
/// kerning changes, even inside a word.
#[derive(Clone, Copy, Debug)]
struct Placement {
    top: f64,
    left: f64,
    /// The em, and whether the style sets the font size it comes from.
    em: f64,
    sized: bool,
}

impl Placement {
    /// Whether a box placed at `next` goes on the line this one starts,
    /// which took in `glyphs`: its top within half an em, as a superscript
    /// stands, and its left further right, no further than the glyphs
    /// reach. Further on, the box stands apart as words set apart do.
    fn goes_on(&self, next: &Placement, glyphs: u64) -> bool {
        (next.top - self.top).abs() <= self.em / 2.0
            && next.left > self.left
            && next.left <= self.left + reach(glyphs, self.em, self.sized)
    }
}

/// How far along a line glyphs set in a font of `em` pixels reach: half an
/// em each, an average of text, and half an em for a space after them,
/// where the font's size is set; 0.6 em each and an em where 16 pixels
/// only stands in for it, which a larger font outgrows.
fn reach(glyphs: u64, em: f64, sized: bool) -> f64 {
    let (each, space) = if sized { (0.5, 0.5) } else { (0.6, 1.0) };
    (glyphs as f64 * each + space) * em
}

/// A CSS length in pixels: `px`, `pt`, `em` (of `em` pixels), `rem`, or a
/// bare zero; `None` for anything else, a percentage among them.
fn pixels(number: &str, em: f64) -> Option<f64> {
    let split = number
        .find(|character: char| character.is_ascii_alphabetic() || character == '%')
        .unwrap_or(number.len());
    let (value, unit) = number.split_at(split);
    let value: f64 = value.parse().ok()?;
    let scale = match unit.to_ascii_lowercase().as_str() {
        "px" => 1.0,
        "pt" => 4.0 / 3.0,
        "em" => em,
        "rem" => 16.0,
        "" if value == 0.0 => 1.0,
        _ => return None,
    };
    Some(value * scale)
}

/// The numbers an element's inline style gives the named properties, as
/// written ("40px"), the last declaration of each winning; `None` for one
/// it does not set to a single number.
fn inline_numbers<const N: usize>(element: &Element, names: [&str; N]) -> [Option<String>; N] {
    let mut numbers: [Option<String>; N] = std::array::from_fn(|_| None);
    let Some(tokens) = element
        .first("style")
        .and_then(|style| tokenize(style).ok())
    else {
        return numbers;
    };
    for declaration in split_top_level(&tokens, &Token::Semicolon) {
        let [Token::Ident(name), rest @ ..] = trim_whitespace(declaration) else {
            continue;
        };
        let [Token::Colon, value @ ..] = trim_whitespace(rest) else {
            continue;
        };
        if let Some(slot) = names
            .iter()
            .position(|wanted| name.eq_ignore_ascii_case(wanted))
        {
            numbers[slot] = match trim_whitespace(value) {
                [Token::Numeric(number)] => Some(number.clone()),
                _ => None,
            };
        }
    }
    numbers
}

/// Where an element's inline style places it (see [`Placement`]); `None`
/// where it does not set `top` and `left` as lengths.
fn placement(element: &Element) -> Option<Placement> {
    let [top, left, size] = inline_numbers(element, ["top", "left", "font-size"]);
    let size = size.and_then(|size| pixels(&size, 16.0));
    let em = size.unwrap_or(16.0);
    Some(Placement {
        top: pixels(&top?, em)?,
        left: pixels(&left?, em)?,
        em,
        sized: size.is_some(),
    })
}

/// Where an SVG text chunk starts, in its text element's space: the place
/// a `text` or `tspan` sets, the glyphs taken in before it, and the em of
/// its font, and whether an attribute or style sets its size.
#[derive(Clone, Copy, Debug)]
struct Pen {
    x: f64,
    y: f64,
    glyphs: u64,
    em: f64,
    sized: bool,
}

/// A single number an SVG attribute gives, in user units or pixels; `None`
/// for a list, a percentage, or another unit.
fn svg_number(value: &str) -> Option<f64> {
    let value = value.trim();
    value.strip_suffix("px").unwrap_or(value).parse().ok()
}

/// The numbers of an SVG list of lengths (`dx="0 0 120 0"`); `None` where
/// one is not a number [`svg_number`] reads.
fn svg_numbers(value: &str) -> Option<Vec<f64>> {
    value
        .split(|character: char| character.is_ascii_whitespace() || character == ',')
        .filter(|part| !part.is_empty())
        .map(svg_number)
        .collect()
}

/// How a glyph an SVG `dx` moves from where the glyphs before it end
/// stands beside them: moved back no further than half an em, as a kerning
/// pair is, it may touch them (`No`); moved on no further, it may stand
/// apart (`Maybe`); further either way, over or past the glyphs before it,
/// it does (`Yes`).
fn svg_shift(dx: f64, em: f64) -> Tri {
    if (-em / 2.0..=0.0).contains(&dx) {
        Tri::No
    } else if dx > 0.0 && dx <= em / 2.0 {
        Tri::Maybe
    } else {
        Tri::Yes
    }
}

/// How an SVG `text` or `tspan`'s `dx` moves its glyphs, one value for
/// each, first to last: the first glyph goes where the element's own place
/// sets it (see [`svg_text_flow`]), `No` here, and each after it as its
/// value moves it from where the one before ends (see [`svg_shift`]). A
/// reader takes the value for each glyph from the innermost element whose
/// list reaches it (see [`add_positioned`]). Empty where the element sets
/// no `dx` that reads.
fn svg_glyph_shifts(element: &Element, em: f64) -> Vec<Tri> {
    let Some(numbers) = element.first("dx").and_then(svg_numbers) else {
        return Vec::new();
    };
    let Some((_, after)) = numbers.split_first() else {
        return Vec::new();
    };
    std::iter::once(Tri::No)
        .chain(after.iter().map(|dx| svg_shift(*dx, em)))
        .collect()
}

/// Add text inside an SVG label whose `dx` lists move glyphs one by one
/// (see [`svg_glyph_shifts`]) to the run, each glyph a list moves apart
/// from the one before starting a piece of its own; whether it runs into
/// the text before it. A reader numbers the characters of the label as
/// they come, white space it collapses left out, and each open element's
/// list gives the next of its values to each, until it runs out.
fn add_positioned(run: &mut Run, text: &str, open: &mut [Open]) -> bool {
    let mut lists: Vec<&mut Open> = open
        .iter_mut()
        .rev()
        .filter(|state| state.shifts_glyphs())
        .collect();
    let mut space = run.svg_label_start || run.last.is_none_or(char::is_whitespace);
    let mut fused = false;
    let mut start = 0;
    for (at, character) in text.char_indices() {
        if lists.is_empty() {
            break;
        }
        if character.is_whitespace() && space {
            continue;
        }
        space = character.is_whitespace();
        let mut shift = Tri::No;
        let mut innermost = true;
        lists.retain_mut(|state| {
            if std::mem::take(&mut innermost) {
                shift = state.glyph_shifts[state.shifts_taken];
            }
            state.shifts_taken += 1;
            state.shifts_glyphs()
        });
        if shift != Tri::No {
            if at > start {
                fused |= run.add(&text[start..at]);
                start = at;
            }
            run.mark(shift);
        }
    }
    fused | run.add(&text[start..])
}

/// The font size, in pixels, an SVG element's inline style or attribute
/// sets.
fn svg_font_size(element: &Element) -> Option<f64> {
    let [size] = inline_numbers(element, ["font-size"]);
    size.and_then(|size| pixels(&size, 16.0))
        .or_else(|| element.first("font-size").and_then(svg_number))
}

/// How a reader sets an SVG `text`, `tspan`, or `textPath` beside the text
/// before it. A `text` sets a label of its own, from the place its `x` and
/// `y` give the pen. A `tspan` placed anew goes on its label's line where
/// it stays on the line (`y` within half an em) and starts where the glyphs
/// since the pen reach (0.6 em each and an em for a space), as a kerning
/// pair or a change of style is set, and may then touch the text before
/// it; set on another line (a `y` beyond, or a `dy` with it), or further
/// along, as a separate label is, it stands apart. A `tspan` moved along
/// the line from where the glyphs before it end (`dx`, of a list its
/// first) stands as [`svg_shift`] tells, and may touch them moved off it
/// as a superscript is (`dy`). A `textPath` sets its glyphs along its
/// path, from where the path and its offset start them, and the glyphs
/// after it go on from the path's end: it stands apart from both, as a
/// label of its own. `em` is the element's font's.
fn svg_text_flow(
    run: &mut Run,
    element: &Element,
    glyphs: u64,
    font: Option<f64>,
    own: Flow,
) -> Flow {
    let (em, sized) = (font.unwrap_or(16.0), font.is_some());
    let number = |name: &str| element.first(name).map(svg_number);
    let (x, y, dy) = (number("x"), number("y"), number("dy"));
    let dx = element
        .first("dx")
        .map(|value| svg_numbers(value).and_then(|numbers| numbers.first().copied()));
    if element.local == "textPath" {
        run.svg_pen = None;
        return Flow::Block;
    }
    if element.local == "text" {
        run.svg_label_start = true;
        run.svg_pen = match (x.unwrap_or(Some(0.0)), y.unwrap_or(Some(0.0))) {
            (Some(x), Some(y)) => Some(Pen {
                x,
                y,
                glyphs,
                em,
                sized,
            }),
            _ => None,
        };
        return Flow::Block;
    }
    let pen = run.svg_pen.take();
    if x.is_none() && y.is_none() {
        return match (dx, dy) {
            (None, None) => {
                run.svg_pen = pen;
                own
            }
            (Some(Some(dx)), None) => match svg_shift(dx, em) {
                Tri::No => own,
                Tri::Maybe => Flow::MaybeApart,
                Tri::Yes => Flow::Block,
            },
            (None, Some(Some(_))) => Flow::MaybeApart,
            _ => Flow::Block,
        };
    }
    let y = match y {
        None => pen.map(|pen| pen.y),
        Some(y) => y,
    };
    let placed_x = x.flatten().map(|x| x + dx.flatten().unwrap_or(0.0));
    if let (Some(x), Some(y)) = (placed_x, y) {
        run.svg_pen = Some(Pen {
            x,
            y,
            glyphs,
            em,
            sized,
        });
    }
    let (Some(pen), Some(y)) = (pen, y) else {
        return Flow::Block;
    };
    let same_line = (y - pen.y).abs() <= pen.em / 2.0 && dy.is_none_or(|dy| dy == Some(0.0));
    let end = pen.x + reach(glyphs - pen.glyphs, pen.em, pen.sized);
    let moved_apart = dx.flatten().is_some_and(|dx| svg_shift(dx, em) == Tri::Yes);
    match (x, placed_x) {
        // Moved up or down from where the glyphs before it end.
        (None, _) if same_line && !moved_apart => Flow::MaybeApart,
        (Some(_), Some(x)) if same_line && x > pen.x && x <= end => Flow::MaybeApart,
        _ => Flow::Block,
    }
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
    /// A positioned box ended where its inline style places it, having
    /// taken in these glyphs: what comes next starts a line of its own,
    /// unless it is a sibling placed where that line goes on.
    after_placed: Option<(Placement, u64)>,
    /// Where the current chunk of an SVG text label starts, where known.
    svg_pen: Option<Pen>,
    /// An SVG text label started and no text came yet: a reader collapses
    /// the white space it starts with (see [`add_positioned`]).
    svg_label_start: bool,
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

    /// Whether the text taken in ends an amount: a digit, or a sign after
    /// one ("12%").
    fn ends_amount(&self) -> bool {
        match self.last {
            Some(last) if last.is_numeric() => true,
            Some(last) => amount_sign(last) && self.before_last.is_some_and(char::is_numeric),
            None => false,
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
    /// which read as one number, count as joined. Characters a reader shows
    /// nothing for, which AnyDoc keeps, stand between nothing: digits on
    /// either side of a word joiner read as one number, and a sign before a
    /// directional mark meets the digits after it.
    fn take(&mut self, text: &str, alt: bool) -> bool {
        let kept = || {
            text.chars()
                .filter(|character| anydoc_keeps(*character) && !invisible(*character))
        };
        let Some(last) = kept().next_back() else {
            return false;
        };
        if kept().all(char::is_whitespace) {
            self.space(' ');
            if matches!(self.sign_before, Some(Sign::Between | Sign::Separator)) {
                self.sign_before = None;
            }
            return false;
        }
        // Text in the flow after a positioned box starts a line of its own.
        if self.after_placed.take().is_some() {
            self.boundary = true;
        }
        let at_space = self.at_space();
        let mut from_first = kept().skip_while(|character| at_space && character.is_whitespace());
        let (first, second) = (from_first.next(), from_first.next());
        // A separator joins only the digits right after it, and what else a
        // box shows parts only the digits it stands between; a sign reads
        // with the amount after white space too.
        let digit_next = || kept().next().is_some_and(char::is_numeric);
        let meets_sign = match self.sign_before.take() {
            Some(Sign::Between) => self.last.is_some_and(char::is_numeric) && digit_next(),
            Some(Sign::Separator) => digit_next(),
            Some(Sign::Dash { .. } | Sign::Amount) => starts_amount(kept()),
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
        self.svg_label_start = false;
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
    /// It holds an element.
    has_elements: bool,
    position: Position,
}

/// What the pass before the walk finds in a chapter: the facts of each
/// element in document order, and the ids an SVG element may refer to,
/// whose elements the walk notes as resources (see [`Resources`]).
#[derive(Default)]
struct ChapterFacts {
    elements: Vec<Facts>,
    /// The ids an SVG `use` element names.
    used: std::collections::HashSet<String>,
    /// The ids a fill, stroke, clip path, mask, or marker names
    /// (`url(#id)`) in an SVG element's attribute or inline style; those a
    /// stylesheet names, the cascade holds. An HTML element is not read:
    /// a resource only it refers to, as a clip path on a box, counts as
    /// unpainted.
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

/// Where [`add_painted`] stands in the tokens it reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaintedScan {
    /// Before a declaration's name.
    Start,
    /// After a name, and whether it names a painting property.
    Name(bool),
    /// In a declaration's value, and whether the property paints.
    Value(bool),
}

/// Add to `ids` the ids that painting declarations name through `url(#id)`
/// references: in a `style` attribute's declarations, or rules, or, with
/// `value`, in a presentation attribute's value. A
/// declaration is a name and a colon, then the tokens up to the next `;`,
/// `{`, or `}`. A reference is a `url` token, or a `url()` function holding
/// a string, as a reader tokenizes them: a URL a reader rejects, such as
/// one that meets an opening parenthesis, names nothing. The tokens are
/// read one by one, not held, so text costs time and memory in proportion
/// to its length, however many references it opens.
fn add_painted(css: &str, value: bool, ids: &mut std::collections::HashSet<String>) {
    let mut tokenizer = Tokenizer::new(css);
    let mut scan = if value {
        PaintedScan::Value(true)
    } else {
        PaintedScan::Start
    };
    // A `url(` function waits for the string it holds.
    let mut url_function = false;
    while let Some(token) = tokenizer.next_token() {
        match (scan, token) {
            (_, Token::Whitespace) => {}
            (_, Token::Semicolon | Token::OpenCurly | Token::CloseCurly) if !value => {
                scan = PaintedScan::Start;
                url_function = false;
            }
            (PaintedScan::Start, Token::Ident(name)) => {
                let painting = PAINTING_PROPERTIES
                    .iter()
                    .any(|property| name.eq_ignore_ascii_case(property));
                scan = PaintedScan::Name(painting);
            }
            (PaintedScan::Name(painting), Token::Colon) => scan = PaintedScan::Value(painting),
            (PaintedScan::Value(true), Token::Url(target)) => {
                ids.extend(fragment(&target).map(str::to_string));
            }
            (PaintedScan::Value(true), Token::Function(function)) => {
                url_function = function.eq_ignore_ascii_case("url");
            }
            (PaintedScan::Value(true), Token::Str(target)) if url_function => {
                ids.extend(fragment(&target).map(str::to_string));
                url_function = false;
            }
            (PaintedScan::Value(_), _) => url_function = false,
            // A selector, or anything else before a declaration.
            _ => scan = PaintedScan::Start,
        }
    }
}

/// The id a URL names in the same document (`#id`), if it names one.
fn fragment(target: &str) -> Option<&str> {
    target.strip_prefix('#').filter(|id| !id.is_empty())
}

/// An open element in the pass before the walk, or the chapter's top
/// level: its element children so far, each with the name it carries, and
/// how many carry each name; and whether its children sit in an SVG image.
#[derive(Default)]
struct Family {
    fact: Option<usize>,
    children: Vec<(u32, u32)>,
    names: HashMap<String, u32>,
    counts: Vec<u32>,
    svg: bool,
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
/// of AnyDoc's blocks, or any element, and where it sits among its
/// siblings; and the ids an SVG element may refer to.
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
        // Only an SVG element paints a resource through an attribute.
        if svg {
            for attribute in element.attributes().flatten() {
                let key = attribute.key.as_ref();
                if key == b"xmlns" || key.starts_with(b"xmlns:") {
                    continue;
                }
                let key = super::xml_local_name(key);
                let used = key == b"href" && local == "use";
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
                    .unwrap_or_else(|_| {
                        String::from_utf8_lossy(attribute.value.as_ref()).into_owned()
                    });
                if used {
                    if let Some(id) = value.trim().strip_prefix('#') {
                        found.used.insert(id.to_string());
                    }
                } else {
                    add_painted(&value, !styled, &mut found.painted);
                }
            }
        }
        if let Some(parent) = family.fact {
            facts[parent].has_blocks |= anydoc_block(&local);
            facts[parent].has_elements = true;
        }
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
            has_elements: false,
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

/// SVG elements a reader draws where they stand, which may refer to a
/// resource to draw it: those that paint what they hold, shapes, images,
/// and `use` elements.
fn svg_graphics(local: &str) -> bool {
    svg_paints(local)
        || matches!(
            local,
            "use"
                | "rect"
                | "circle"
                | "ellipse"
                | "line"
                | "polyline"
                | "polygon"
                | "path"
                | "image"
        )
}

/// Whether an SVG element draws anything a resource it refers to could
/// show in: a shape of some size, text, an image, a `use` element, or an
/// element holding others. A rectangle, circle, or ellipse sized zero, a
/// path whose points are empty, and an empty group, draw nothing; one
/// whose size or points no attribute gives may take them from a style.
fn svg_draws(element: &Element, has_children: bool, has_elements: bool) -> bool {
    let zero = |name: &str| element.first(name).and_then(svg_number) == Some(0.0);
    let empty = |name: &str| {
        element
            .first(name)
            .is_some_and(|value| value.trim().is_empty())
    };
    match element.local.as_str() {
        "use" | "line" | "image" => true,
        "path" => !empty("d"),
        "polyline" | "polygon" => !empty("points"),
        "rect" => !zero("width") && !zero("height"),
        "circle" => !zero("r"),
        "ellipse" => !zero("rx") && !zero("ry"),
        "text" | "tspan" | "textPath" => has_children,
        _ => has_elements,
    }
}

/// SVG resources a reader paints only where a fill, stroke, clip, mask,
/// or marker names them.
fn svg_painted_resource(local: &str) -> bool {
    matches!(local, "pattern" | "clipPath" | "mask" | "marker")
}

/// Whether a reader paints what an element holds where it stands, as far
/// as references to SVG resources decide it (see [`Resources`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Paint {
    Yes,
    No,
    /// Only where a resource it stands in is drawn: the last of the links
    /// (see [`Resources::links`]) up to where the image paints for certain.
    If(u32),
}

/// The resources of a chapter's SVG images, which a reader paints only
/// where something it renders refers to them: a `symbol`, or an element
/// inside `defs`, where a `use` element draws it, and a pattern, clip
/// path, mask, or marker where a fill, stroke, clip, mask, or marker names
/// it. Gradients, filters, and elements SVG does not define paint nothing.
/// A reference may come before or after what it names, and what refers to
/// one resource may stand inside another, so the walk notes each resource
/// and each reference as it comes, and settles them as it ends (see
/// [`Resources::settle`]).
#[derive(Default)]
struct Resources {
    /// By the id a reference names, and whether a painting property or a
    /// `use` element names it: the slot of what it names.
    slots: HashMap<(bool, Rc<str>), u32>,
    /// By slot: whether something a reader certainly renders refers to it.
    live: Vec<bool>,
    /// A resource that paints where its slot is live, or where the element
    /// it stands in paints.
    links: Vec<(u32, Paint)>,
    /// References from elements a reader renders only where a link paints:
    /// the link, and the slot the reference names.
    edges: Vec<(u32, u32)>,
    /// Links text a reader hides stands under, unless the link paints.
    pending: Vec<u32>,
    /// By slot: whether a `symbol` or `svg` element takes it, whose
    /// viewport a `use` element sized zero draws nothing in.
    viewport: Vec<bool>,
    /// References from `use` elements sized zero, which draw only what is
    /// not such an element: where the element stands, and the slot.
    sized_zero: Vec<(Paint, u32)>,
    /// The ids references may name that an element carried already: a
    /// reference names the first element that carries its id.
    taken: std::collections::HashSet<Rc<str>>,
}

impl Resources {
    fn slot(&mut self, painting: bool, id: &str) -> u32 {
        let next = self.live.len() as u32;
        let slot = *self.slots.entry((painting, Rc::from(id))).or_insert(next);
        if slot == next {
            self.live.push(false);
            self.viewport.push(false);
        }
        slot
    }

    /// What paints where a resource in `slot` is drawn, or where `outer`
    /// paints.
    fn either(&mut self, slot: u32, outer: Paint) -> Paint {
        if outer == Paint::Yes || self.live[slot as usize] {
            return Paint::Yes;
        }
        self.links.push((slot, outer));
        Paint::If(self.links.len() as u32 - 1)
    }

    /// How an element inside an SVG image, which its parent places where
    /// `placed` says, stands: whether a reader draws it where it stands,
    /// or as an instance a `use` element draws, and whether it paints what
    /// it holds. A reference names it only where it is the `first` element
    /// with its id; `facts` and `reader` tell what references may name.
    fn svg_element(
        &mut self,
        element: &Element,
        placed: Paint,
        first: bool,
        facts: &ChapterFacts,
        reader: &Cascade,
    ) -> (Paint, Paint) {
        let local = element.local.as_str();
        let painting = svg_painted_resource(local);
        let slot = element.first("id").filter(|_| first).and_then(|id| {
            let named = if painting {
                facts.painted.contains(id) || reader.may_paint(id)
            } else {
                facts.used.contains(id)
            };
            named.then(|| self.slot(painting, id))
        });
        if let (Some(slot), "symbol" | "svg") = (slot, local) {
            self.viewport[slot as usize] = true;
        }
        let position = match slot {
            Some(slot) if !painting => self.either(slot, placed),
            _ => placed,
        };
        let paint = if painting || local == "symbol" {
            slot.map_or(Paint::No, |slot| self.either(slot, Paint::No))
        } else if svg_paints(local) {
            position
        } else {
            Paint::No
        };
        (position, paint)
    }

    /// Whether an element carries an id a reference may name that no
    /// element before it carried; noted for every element, as a reference
    /// names the first.
    fn first_with_id(&mut self, element: &Element, facts: &ChapterFacts, reader: &Cascade) -> bool {
        let Some(id) = element.first("id") else {
            return false;
        };
        let named = facts.used.contains(id) || facts.painted.contains(id) || reader.may_paint(id);
        named && self.taken.insert(Rc::from(id))
    }

    /// Note a reference from an element a reader renders where `position`
    /// says: a `use` element `sized_zero` draws only what has no viewport
    /// of its own, which the walk knows only once it has met what it names.
    fn refer(&mut self, painting: bool, id: &str, position: Paint, sized_zero: bool) {
        let slot = self.slot(painting, id);
        if sized_zero {
            self.sized_zero.push((position, slot));
            return;
        }
        match position {
            Paint::Yes => self.live[slot as usize] = true,
            Paint::No => {}
            Paint::If(link) => self.edges.push((link, slot)),
        }
    }

    /// Note text a reader hides unless `link` paints.
    fn pending(&mut self, link: u32) {
        if self.pending.last() != Some(&link) {
            self.pending.push(link);
        }
    }

    /// Settle what paints once the walk has noted every resource and
    /// reference: a slot is live where something certainly rendered refers
    /// to it, or something rendered under a link that paints; a link paints
    /// where its slot is live or what it stands in paints. Whether text
    /// under a link that does not paint remains.
    fn settle(&self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let mut live = self.live.clone();
        let mut painted = vec![false; self.links.len()];
        let mut by_slot: Vec<Vec<u32>> = vec![Vec::new(); live.len()];
        let mut inside: Vec<Vec<u32>> = vec![Vec::new(); self.links.len()];
        for (link, &(slot, outer)) in self.links.iter().enumerate() {
            by_slot[slot as usize].push(link as u32);
            if let Paint::If(outer) = outer {
                inside[outer as usize].push(link as u32);
            }
        }
        let mut from: Vec<Vec<u32>> = vec![Vec::new(); self.links.len()];
        for &(link, slot) in &self.edges {
            from[link as usize].push(slot);
        }
        for &(position, slot) in &self.sized_zero {
            match position {
                _ if self.viewport[slot as usize] => {}
                Paint::Yes => live[slot as usize] = true,
                Paint::No => {}
                Paint::If(link) => from[link as usize].push(slot),
            }
        }
        let mut slots: Vec<u32> = (0..live.len() as u32)
            .filter(|slot| live[*slot as usize])
            .collect();
        let mut links: Vec<u32> = Vec::new();
        while let Some(slot) = slots.pop() {
            links.extend(&by_slot[slot as usize]);
            while let Some(link) = links.pop() {
                if std::mem::replace(&mut painted[link as usize], true) {
                    continue;
                }
                links.extend(&inside[link as usize]);
                for &named in &from[link as usize] {
                    if !std::mem::replace(&mut live[named as usize], true) {
                        slots.push(named);
                    }
                }
            }
        }
        self.pending.iter().any(|link| !painted[*link as usize])
    }
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
    /// The size of its font there, where its attributes or inline style,
    /// or those of an element around it, set it.
    svg_font: Option<f64>,
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
        svg_font,
    } = *meeting;
    let local = element.local.as_str();
    let spliced = !run.splices.is_empty();
    // How a reader lays the element out beside the text around it. Each
    // text label of an SVG image is a block of its own, and a flex or grid
    // item stands where its row sets it (see [`Row`]), floated or not;
    // otherwise its style decides, or, where it was not read (the element
    // holds no text), the reader's defaults.
    let own = match style {
        Some(style) => style.flow(),
        None if reader_block_by_default(element) => Flow::Block,
        None => Flow::Inline,
    };
    // A span of an SVG label meets the text before it as its place says
    // (see [`svg_text_flow`]); the text after it goes on from where its
    // glyphs end. Glyphs set along a path stand as a label of their own.
    if in_svg && matches!(local, "text" | "tspan" | "textPath") {
        let junction = svg_text_flow(run, element, glyphs.0, svg_font, own);
        if local == "tspan" {
            run.mark(match junction {
                Flow::Block => Tri::Yes,
                Flow::MaybeApart => Tri::Maybe,
                _ => Tri::No,
            });
        }
    }
    let flow = match (parent_items, own) {
        _ if in_svg && matches!(local, "text" | "textPath") => Flow::Block,
        (Tri::Yes, Flow::Positioned) => Flow::Positioned,
        (Tri::Yes, _) => Flow::Inline,
        (_, Flow::Block) => Flow::Block,
        (Tri::Maybe, _) => Flow::MaybeApart,
        _ => own,
    };
    let (breaks_before, breaks_after) = style.map_or((Tri::No, Tri::No), |style| {
        (style.before.breaks, style.after.breaks)
    });
    // A positioned box before this one, its sibling, left a line: this one
    // goes on it where its inline style places it there, perhaps touching
    // the text before it; anything else starts a line of its own.
    let placed_at = (flow == Flow::Positioned)
        .then(|| placement(element))
        .flatten();
    if let Some((before, glyphs)) = run.after_placed.take() {
        let goes_on = placed_at.is_some_and(|here| before.goes_on(&here, glyphs));
        run.mark(if goes_on { Tri::Maybe } else { Tri::Yes });
    }
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
                placed: placed_at,
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
    let lost_sign = effects.sign_after.is_some_and(Sign::of_amount)
        && runs.last().is_some_and(Run::ends_amount);
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
    // The line a positioned box inside the element left goes on no further
    // than the element.
    if run.after_placed.take().is_some() {
        run.boundary = true;
    }
    match effects.glyphs_at {
        Some(start) => match (drop_cap(start, glyphs), start.placed) {
            (Some(cap), _) => run.beside_float = Some(cap),
            (None, Some(placed)) => run.after_placed = Some((placed, glyphs.0 - start.glyphs.0)),
            (None, None) => run.mark(effects.boundary_after),
        },
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

/// End an element: an item's right edge, and the lines of its own items,
/// stand before the next item; those of an inline box laying out items,
/// before the text after it. Then apply what it does to AnyDoc's inline run
/// (see [`end_element`]).
fn close_element(
    open: &mut [Open],
    mut closed: Open,
    runs: &mut Vec<Run>,
    glyphs: (u64, u64),
) -> bool {
    let trailing = match closed.items {
        Tri::Yes => closed.row.trailing(),
        _ => Tri::No,
    };
    let right = closed.edges.1.max(trailing);
    match open.last_mut() {
        Some(parent) if closed.passes_row => parent.row = closed.row,
        Some(parent) if closed.item => parent.row.last_right = right,
        _ => {}
    }
    if closed.inline_items && !closed.item {
        closed.effects.boundary_after = closed.effects.boundary_after.max(right);
    }
    end_element(&closed.effects, runs, glyphs)
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
    let mut resources = Resources::default();
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
                        reader.note_sibling(&elements, &mut earlier, &ancestors, work)?;
                    }
                }
                if let Some(closed) = open.pop() {
                    found.drops_shown |= close_element(&mut open, closed, &mut runs, glyphs);
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
            quick_xml::events::Event::Eof => {
                found.converts_hidden |= resources.settle();
                return Ok(found);
            }
            _ => {
                buffer.clear();
                continue;
            }
        };
        if let Some(text) = text {
            // Text straight inside a box laying out items stands as an item
            // of its own, unless it goes on the text before it; white space
            // alone a reader leaves out.
            let apart = match open.last_mut() {
                Some(state)
                    if state.items == Tri::Yes
                        && !text
                            .chars()
                            .all(|character| character.is_ascii_whitespace()) =>
                {
                    state.row.text()
                }
                _ => Tri::No,
            };
            if let (Some(state), Some(run)) = (open.last(), runs.last_mut()) {
                // `pre` text, or a display formula's, inside a link joins the
                // run around it.
                let whole = run.splices.last().map_or(Whole::No, |splice| splice.whole);
                let taken = match (state.reach, whole) {
                    (Reach::Walk, _) | (Reach::Whole, Whole::Pre) => Some(text.as_str()),
                    (Reach::Whole, Whole::Tex) => Some(text.trim()),
                    _ => None,
                };
                let positioned = state.svg_text && open.iter().any(Open::shifts_glyphs);
                if let Some(taken) = taken {
                    run.mark(apart);
                    count_glyphs(&mut glyphs, taken);
                    found.fuses_blocks |= if positioned {
                        add_positioned(run, taken, &mut open)
                    } else {
                        run.add(taken)
                    };
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
                    || state.paint == Paint::No
                    || (state.in_svg && !state.svg_text);
                let shown_anyway = match state.exempt {
                    Exempt::None => false,
                    Exempt::Description => true,
                    Exempt::RubyParenthesis => !text.chars().any(char::is_alphanumeric),
                };
                match state.reach {
                    Reach::Walk | Reach::Whole if hidden => {
                        found.converts_hidden |= !shown_anyway;
                    }
                    // Text in a resource converts hidden unless something a
                    // reader renders draws the resource, which the walk
                    // settles as it ends.
                    Reach::Walk | Reach::Whole => {
                        if let (Paint::If(link), false) = (state.paint, shown_anyway) {
                            resources.pending(link);
                        }
                    }
                    Reach::Dropped => {}
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
        let parent_in_svg = open.last().is_some_and(|parent| parent.in_svg);
        let svg_element = parent_in_svg || element.lower == "svg";
        let first_id = resources.first_with_id(element, &facts, reader);
        // What an SVG element refers to: the element a `use` element draws
        // (by `href`, before `xlink:href`), and the resources its painting
        // properties name.
        let references: Vec<(bool, Rc<str>)> = if svg_element {
            let mut references: Vec<(bool, Rc<str>)> = Vec::new();
            if element.local == "use" {
                let href = element
                    .values("href")
                    .find(|(prefixed, _)| !prefixed)
                    .or_else(|| element.values("href").next());
                if let Some(id) = href.and_then(|(_, href)| fragment(href.trim())) {
                    references.push((false, Rc::from(id)));
                }
            }
            let may_paint = reader.paints_resources()
                || PAINTING_PROPERTIES
                    .iter()
                    .chain(&["style"])
                    .any(|name| element.values(name).next().is_some());
            if may_paint {
                let tree = Tree {
                    stack: &elements,
                    earlier: &earlier,
                };
                references.extend(
                    reader
                        .references(&tree, &ancestors, work)?
                        .into_iter()
                        .map(|id| (true, id)),
                );
            }
            references
        } else {
            Vec::new()
        };
        // The reader's style: for the text below the element, for whether a
        // reader shows an element AnyDoc skips, a line break, or an image,
        // and how it lays them out, for the `::before` and `::after` boxes
        // of an empty element, and for whether a reader renders an SVG
        // element that refers to a resource. Nothing below a dropped or
        // empty element converts or shows.
        let style = if (has_children && reach != Reach::Dropped)
            || (anydoc_hidden && (reach == Reach::Omitted || matches!(local.as_str(), "br" | "hr")))
            || (parent_reach == Some(Reach::Walk)
                && matches!(local.as_str(), "br" | "img" | "image"))
            || (reach != Reach::Dropped && reader.styles_pseudo_boxes())
            || !references.is_empty()
        {
            let tree = Tree {
                stack: &elements,
                earlier: &earlier,
            };
            Some(reader.evaluate(&tree, &ancestors, work)?)
        } else {
            None
        };
        if let Some(style) = &style {
            element.container.set(style.container);
        }
        // How the element meets AnyDoc's inline run. Paragraphs, headings,
        // quotes, list items, cells, captions, and the body get runs of
        // their own; lists, tables, `pre`, rules, and containers holding
        // blocks end the current paragraph; everything else is walked
        // inline, although a reader starts a new line for a container, and
        // for anything its style makes a block. A link's content joins the
        // run around it (see [`Splice`]).
        let spliced = runs.last().is_some_and(|run| !run.splices.is_empty());
        let parent_items = open.last().map_or(Tri::No, |parent| parent.items);
        let svg_font = svg_element
            .then(|| svg_font_size(element).or(open.last().and_then(|parent| parent.svg_font)))
            .flatten();
        // A flex or grid item meets the item before it as its row sets it
        // (see [`Row`]), and an inline box laying out items meets the text
        // before it as its own left edge does. A positioned child leaves the
        // row, and one without a box (`display: contents`) passes it on to
        // what it holds.
        let item = parent_items == Tri::Yes
            && style
                .as_ref()
                .is_none_or(|style| style.flow() != Flow::Positioned && !style.contents);
        let inline_items = !parent_in_svg
            && element.lower != "svg"
            && style
                .as_ref()
                .is_some_and(|style| style.items == Tri::Yes && style.inline == Tri::Yes);
        let item_box = if item || inline_items {
            let tree = Tree {
                stack: &elements,
                earlier: &earlier,
            };
            reader.item_box(&tree, &ancestors, work)?
        } else {
            ItemBox::default()
        };
        let edges = (item_box.left, item_box.right);
        let apart = match open.last_mut() {
            Some(parent) if item => parent.row.next(item_box),
            _ if inline_items => item_box.left,
            _ => Tri::No,
        };
        if let Some(run) = runs.last_mut() {
            run.mark(apart);
        }
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
                    svg_font,
                },
                &mut glyphs,
                &mut found,
            ),
            _ => Effects::default(),
        };
        let element = elements.last().expect("the element just pushed");
        // Whether a reader paints what the element holds where it stands:
        // inside an SVG image, see [`Resources`]; of a `switch`'s children,
        // one it renders at most.
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
        let in_svg = svg_element;
        let parent_paint = parent.map_or(Paint::Yes, |parent| parent.paint);
        let (position, paint) = if parent_in_svg {
            let placed = if rendered == Tri::Yes {
                parent_paint
            } else {
                Paint::No
            };
            resources.svg_element(element, placed, first_id, &facts, reader)
        } else {
            (parent_paint, parent_paint)
        };
        let svg_text = parent.is_some_and(|parent| parent.svg_text)
            || (parent_in_svg && element.local == "text");
        let switch_taken = (in_svg && element.local == "switch").then_some(false);
        // A `dx` list is read where it, or one around it, moves a glyph
        // apart.
        let glyph_shifts = if parent_in_svg && matches!(element.local.as_str(), "text" | "tspan") {
            let shifts = svg_glyph_shifts(element, svg_font.unwrap_or(16.0));
            let read = shifts.iter().any(|shift| *shift != Tri::No)
                || open.iter().any(Open::shifts_glyphs);
            if read {
                shifts
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
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
                transparent: false,
                in_svg: children_in_svg,
                paint,
                svg_text,
                switch_taken,
                svg_font,
                glyph_shifts,
                shifts_taken: 0,
                items: Tri::No,
                row: Row::default(),
                item,
                inline_items: false,
                edges,
                passes_row: false,
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
                let (items, row) = match parent {
                    Some(parent) if style.contents => (parent.items, parent.row),
                    _ if children_in_svg => (Tri::No, Row::default()),
                    _ => {
                        let row = Row {
                            layout: style.item_layout,
                            ..Row::default()
                        };
                        (style.items, row)
                    }
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
                    transparent: parent.is_some_and(|parent| parent.transparent)
                        || style.transparent != Tri::No,
                    in_svg: children_in_svg,
                    paint,
                    svg_text,
                    switch_taken,
                    svg_font,
                    glyph_shifts,
                    shifts_taken: 0,
                    items,
                    row,
                    item,
                    inline_items,
                    edges,
                    passes_row: style.contents,
                    exempt,
                    effects,
                }
            }
        };
        // What a shown `::before` or `::after` box adds, which AnyDoc never
        // converts: text, such as a label, a sign beside digits, and what
        // keeps digits on either side apart. A box takes the element's
        // visibility unless its own settles it. AnyDoc writes a list item
        // with a list marker of its own, which stands in for a bullet the
        // item's `::before` box shows, a hyphen or dash set apart from the
        // item's text; any other sign on an item is lost as it is elsewhere.
        // A reference counts where a reader renders what refers: shown, not
        // transparent, drawing something, and standing where the image
        // paints, or in a resource that paints where drawn.
        if !references.is_empty()
            && !state.undisplayed
            && !state.invisible
            && !state.transparent
            && svg_draws(element, has_children, fact.has_elements)
        {
            let drawn = if svg_graphics(&element.local) {
                position
            } else {
                paint
            };
            let zero = |name: &str| element.first(name).and_then(svg_number) == Some(0.0);
            let sized_zero = element.local == "use" && (zero("width") || zero("height"));
            for (painting, id) in &references {
                resources.refer(*painting, id, drawn, sized_zero);
            }
        }
        let hidden = state.undisplayed || state.contents_hidden || state.paint == Paint::No;
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
        let sign_before = generated.and_then(|style| {
            style
                .before
                .sign
                .filter(|_| seen(&style.before) && !(list_item && style.before.bullet))
        });
        effects.sign_after =
            generated.and_then(|style| style.after.sign.filter(|_| seen(&style.after)));
        state.effects.sign_after = effects.sign_after;
        // A sign before the element's content sits before the text after
        // it, and after the text the line holds before it.
        let mark_sign = |runs: &mut Vec<Run>| {
            let Some(run) = runs.last_mut().filter(|_| sign_before.is_some()) else {
                return false;
            };
            run.sign_before = run.sign_before.max(sign_before);
            sign_before.is_some_and(Sign::of_amount) && !run.boundary && run.ends_amount()
        };
        // An empty element holds no text to check.
        if !has_children {
            if let Some(closed) = elements.pop() {
                ancestors.pop(&closed);
                earlier.pop();
                if let Some(siblings) = earlier.last_mut() {
                    siblings.push(closed);
                    reader.note_sibling(&elements, &mut earlier, &ancestors, work)?;
                }
            }
            state.effects.opens_run = false;
            found.drops_shown |= mark_sign(&mut runs);
            found.drops_shown |= close_element(&mut open, state, &mut runs, glyphs);
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
        reader.push_sheet(&parse_stylesheet(sheet).expect("stylesheet"), Applies::Yes);
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
    fn sibling_rules_settled_by_one_sibling_cost_nothing_after() {
        // Thousands of `~` rules any sibling fits, whether their compound
        // requires a key or none, over a long run of siblings: the first
        // settles each, and the rest do not try them again.
        for step in ["*", "i"] {
            let sheet: String = (0..4000)
                .map(|rule| format!("{step} ~ b{rule} {{ display: block }}\n"))
                .collect();
            let (reader, anydoc) = cascade_for(&[&sheet]);
            let body = format!("<div>{}</div><p>End.</p>", "<i/>".repeat(20_000));
            let mut work = 0;
            chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("chapter walk");
            // Each parent's children settle each step once: a few tries of
            // 4,000 steps at each depth, not 4,000 for each of 20,000.
            assert!(work <= 100_000, "{step}: {work}");
        }
        // Steps no sibling fits are tried for each one, and the work counts:
        // past the bound, the chapter is refused as a resource limit, not
        // judged on what was recorded before.
        let sheet: String = (0..4000)
            .map(|rule| format!("[data-k] ~ b{rule} {{ display: block }}\n"))
            .collect();
        let (reader, anydoc) = cascade_for(&[&sheet]);
        let body = format!("<div>{}</div><p>End.</p>", "<i/>".repeat(2000));
        let mut work = 0;
        chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("chapter walk");
        assert!(work >= 2000 * 4000, "{work}");
        let mut work = MAX_MATCH_WORK - 4000 * 50;
        assert!(matches!(
            chapter_text(&chapter(&body), &reader, &anydoc, &mut work),
            Err(DocumentError::ResourceLimit)
        ));
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
        // A stylesheet's rule names a resource for the elements it matches.
        assert!(!converts_hidden(
            &[".r { marker-end: url(#k) }"],
            &format!(
                r#"<svg {svg}><marker id="k"><text>Shown</text></marker><path class="r"/></svg>"#
            )
        ));
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
    fn painting_references_are_read_once_through() {
        // `url(` opened again and again, never closed: each is a URL a
        // reader rejects at the next parenthesis, and names nothing.
        let bomb = "url(#".repeat(80_000);
        let mut ids = std::collections::HashSet::new();
        add_painted(&bomb, true, &mut ids);
        add_painted(&format!(".a {{ fill: {bomb} }}"), false, &mut ids);
        add_painted(&format!("fill: {bomb}"), false, &mut ids);
        assert!(ids.is_empty(), "{ids:?}");
        // Closed references name their ids, each kept once; references in
        // properties that paint nothing, and URLs elsewhere, name none.
        add_painted(&"url(#p) ".repeat(80_000), true, &mut ids);
        add_painted(
            r##"rect { fill: url( "#q" ) } .m { MASK: url('#m') } .x { color: url(#c) } a:hover { background: url(#b) }"##,
            false,
            &mut ids,
        );
        add_painted("marker-end: url(#k); stroke: url(x.svg#s)", false, &mut ids);
        let mut found: Vec<&str> = ids.iter().map(String::as_str).collect();
        found.sort_unstable();
        assert_eq!(found, ["k", "m", "p", "q"]);
        // A chapter holding such values is read as any other: its text is
        // painted, and nothing is flagged.
        let svg = r#"xmlns="http://www.w3.org/2000/svg""#;
        let body = format!(
            r#"<p>Figure 1.</p><svg {svg}><style>.a {{ fill: {bomb} }}</style><rect fill="{bomb}" style="clip-path: {bomb}"/><text>Chart</text></svg>"#
        );
        assert_eq!(walk(&[], &body), ChapterText::default());
        let facts = element_facts(&chapter(&body)).expect("chapter facts");
        assert!(facts.painted.is_empty() && facts.used.is_empty());
    }

    #[test]
    fn only_what_a_reader_renders_draws_a_resource() {
        let svg =
            r#"xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink""#;
        let text = r#"<text x="10" y="30">Wire 1,250.00</text>"#;
        let named = r#"<text id="t" x="10" y="30">Wire 1,250.00</text>"#;
        let image = |inner: &str| format!(r#"<p>Figure 1.</p><svg {svg}>{inner}</svg>"#);
        // What refers to a resource draws it only where a reader renders
        // it: shown, not transparent, drawing something, and standing
        // where the image paints, or in a resource something draws; and
        // a rule names one only for the elements it matches.
        for (sheet, inner) in [
            (
                "",
                format!(r##"<defs>{named}</defs><use href="#t" display="none"/>"##),
            ),
            (
                "",
                format!(r##"<defs>{named}</defs><use xlink:href="#t" visibility="hidden"/>"##),
            ),
            ("", format!(r##"<defs>{named}<use href="#t"/></defs>"##)),
            (
                "",
                format!(r##"<defs>{named}</defs><use href="#t" opacity="0"/>"##),
            ),
            (
                ".off { display: none }",
                format!(r##"<defs>{named}</defs><use class="off" href="#t"/>"##),
            ),
            (
                ".nothing { mask: url(#m) }",
                format!(r#"<mask id="m">{text}</mask>"#),
            ),
            (
                "",
                format!(r##"<clipPath id="c">{text}</clipPath><g clip-path="url(#c)"/>"##),
            ),
            (
                "",
                format!(
                    r##"<mask id="m">{text}</mask><rect mask="url(#m)" width="0" height="0"/>"##
                ),
            ),
            (
                "",
                format!(
                    r##"<pattern id="p" width="200" height="50">{text}</pattern><rect fill="url(#p)" width="0" height="40"/>"##
                ),
            ),
            (
                "",
                format!(
                    r##"<clipPath id="c">{text}</clipPath><rect clip-path="url(#c)" width="400" height="60" display="none"/>"##
                ),
            ),
            (
                ".r { fill: url(#p) } .r.plain { fill: black }",
                format!(
                    r#"<pattern id="p">{text}</pattern><rect class="r plain" width="10" height="10"/>"#
                ),
            ),
            (
                "",
                format!(
                    r##"<symbol id="a"><use href="#b"/></symbol><symbol id="b">{text}</symbol>"##
                ),
            ),
            (
                "",
                format!(
                    r##"<defs><symbol id="s" viewBox="0 0 200 50">{text}</symbol></defs><use href="#s" width="0" height="0"/>"##
                ),
            ),
            (
                "",
                format!(r##"<marker id="k">{text}</marker><path marker-start="url(#k)" d=""/>"##),
            ),
        ] {
            let body = image(&inner);
            assert!(converts_hidden(&[sheet], &body), "{sheet} {body}");
        }
        // A reference names the first element with its id.
        assert!(converts_hidden(
            &[],
            &format!(
                r##"<p id="t">Note</p><svg {svg}><defs>{named}</defs><use href="#t"/></svg>"##
            )
        ));
        // What a reader renders draws what it refers to, whatever comes
        // first, and through a resource it stands in.
        for (sheet, inner) in [
            ("", format!(r##"<defs>{named}</defs><use href="#t"/>"##)),
            ("", format!(r##"<use href="#t"/><defs>{named}</defs>"##)),
            (
                "",
                format!(
                    r##"<clipPath id="c">{text}</clipPath><rect clip-path="url(#c)" width="400" height="60" fill="black"/>"##
                ),
            ),
            (
                ".r { fill: url(#p) }",
                format!(
                    r#"<pattern id="p">{text}</pattern><rect class="r" width="10" height="10"/>"#
                ),
            ),
            (
                "",
                format!(
                    r##"<symbol id="a"><use href="#b"/></symbol><symbol id="b">{text}</symbol><use href="#a"/>"##
                ),
            ),
            (
                "",
                format!(r##"<defs>{named}<g id="g"><use href="#t"/></g></defs><use href="#g"/>"##),
            ),
            // A `use` element sized zero still draws what has no viewport.
            (
                "",
                format!(r##"<defs><g id="g">{text}</g></defs><use href="#g" width="0"/>"##),
            ),
        ] {
            let body = image(&inner);
            assert!(!converts_hidden(&[sheet], &body), "{sheet} {body}");
        }
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
        // A word joiner a reader shows nothing for does not keep them apart.
        assert!(fuses(
            &[".x { display: inline } .x:has(b) { display: block }"],
            "<p>A</p><div class=\"x\">Units 12\u{2060}</div><div class=\"x\">50 shipped</div>"
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
    fn floats_links_and_svg_text_keep_their_own_lines() {
        let fuses = |sheets: &[&str], body: &str| walk(sheets, body).fuses_blocks;
        let math = r#"xmlns="http://www.w3.org/1998/Math/MathML""#;
        let svg = r#"xmlns="http://www.w3.org/2000/svg""#;
        for (sheets, body) in [
            // A floated or positioned box holding digits is no drop cap,
            // unless a single figure floated to open its paragraph.
            (
                &[".f { float: left }"][..],
                r#"<p><span class="f">10</span>250 units received</p>"#.to_string(),
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
            // Positioned runs set on the next line, or far along this one,
            // text in the flow after one, and runs whose places do not
            // share a containing box; on one line, digits that meet.
            (
                &[],
                r#"<p><span style="position:absolute;top:0px;left:0px">Balance due</span><span style="position:absolute;top:26px;left:0px">Grand total</span></p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="position:absolute;top:0;left:0">Name</span><span style="position:absolute;top:0;left:300px">Amount</span></p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="position:absolute;top:40px;left:40px">Once</span><span style="position:absolute;top:40px;left:100px">upon</span></p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="position:absolute;top:0;left:0">Balance due</span>Grand total</p>"#.into(),
            ),
            (
                &[],
                r#"<p><b><span style="position:absolute;top:0;left:0">Balance</span></b><span style="position:absolute;top:0;left:70px">Grand</span></p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="position:absolute;top:0;left:0">Units 12</span><span style="position:absolute;top:0;left:60px">50</span></p>"#.into(),
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
            // Spans of a label set on another line, further along it, or
            // moved past half an em; on the line, digits that meet.
            (
                &[],
                format!(
                    r#"<svg {svg}><text x="10" y="30"><tspan x="10" y="30">Collect the forms</tspan><tspan x="10" y="50">Review the totals</tspan></text></svg>"#
                ),
            ),
            (
                &[],
                format!(r#"<svg {svg}><text x="20" y="90" font-size="12">Q1<tspan x="90">Q2</tspan></text></svg>"#),
            ),
            (
                &[],
                format!(
                    r#"<svg {svg}><text font-size="24">Quarterly W<tspan x="136.4" y="0">orkbook</tspan></text></svg>"#
                ),
            ),
            (
                &[],
                format!(r#"<svg {svg}><text x="0" y="20">Balance due<tspan dx="40">1,250.00</tspan></text></svg>"#),
            ),
            (
                &[],
                format!(r#"<svg {svg}><text>Units 12<tspan x="70" y="0">50</tspan></text></svg>"#),
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
            // AnyDoc keeps a line break, beside other text in a link too,
            // and an inline formula's delimiters.
            (
                &[][..],
                "<p>Balance due<span><br/></span>1,250.00</p>".to_string(),
            ),
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
            // A fixed-layout line set as positioned runs, split inside a
            // word where the kerning or the character style changes, and a
            // superscript raised off the line.
            (
                &[],
                r#"<p><span style="position:absolute;top:0px;left:0px">The Wo</span><span style="position:absolute;top:0px;left:62.97px">nderful Morning</span></p>"#.into(),
            ),
            (
                &[],
                r#"<p><span style="position:absolute;top:40px;left:20px">Filed on the 21</span><span style="position:absolute;top:36px;left:140.55px">st </span></p>"#.into(),
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
            // A label's spans placed where the glyphs before them end, as
            // a kerning pair or a change of style is set.
            (
                &[],
                format!(
                    r#"<svg {svg}><text transform="matrix(1 0 0 1 20 40)" font-size="24">T<tspan x="14.06" y="0">ax Year Summary</tspan></text></svg>"#
                ),
            ),
            (
                &[],
                format!(
                    r#"<svg {svg}><g font-size="24"><text><tspan x="0" y="0">Quarterly W</tspan><tspan x="120.01" y="0">orkbook</tspan></text></g></svg>"#
                ),
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
    fn svg_glyphs_moved_back_one_by_one_or_onto_a_path_stand_apart() {
        let fuses = |body: &str| {
            walk(
                &[],
                &format!(
                    r#"<p>Figure 1.</p><svg xmlns="http://www.w3.org/2000/svg" width="500" height="120"><path id="p" d="M10 30 H490"/>{body}</svg>"#
                ),
            )
            .fuses_blocks
        };
        for body in [
            // A span moved back past half an em, over or before the glyphs
            // before it.
            r#"<text x="200" y="30" font-size="16">Units 12<tspan dx="-190">50</tspan></text>"#,
            r#"<text x="200" y="30" font-size="16">Balance due<tspan dx="-190">Grand total</tspan></text>"#,
            // On the line, but moved along it.
            r#"<text x="10" y="30" font-size="16">Balance due<tspan y="30" dx="200">Grand total</tspan></text>"#,
            // A `dx` list moving a glyph apart, from the element it is on or
            // one around it, white space a reader collapses not counted.
            r#"<text x="10" y="30" font-size="16">Units <tspan dx="0 0 120 0">1250</tspan></text>"#,
            r#"<text x="10" y="30" font-size="16" dx="0,0,20,0">  1250</text>"#,
            r#"<text x="10" y="30" font-size="16" dx="0 0 120 0">12<tspan>50</tspan></text>"#,
            // Glyphs set along a path, and those after it, from the path's
            // end.
            r##"<text font-size="16">Units 12<textPath href="#p" startOffset="300">50</textPath></text>"##,
            r##"<text font-size="16"><textPath href="#p">Units 12</textPath>50</text>"##,
        ] {
            assert!(fuses(body), "{body}");
        }
        for body in [
            // Kerned: moved back no further than half an em.
            r#"<text x="10" y="30" font-size="16">Units 1<tspan dx="-1">250</tspan></text>"#,
            r#"<text x="10" y="30" font-size="16">Units 12<tspan dx="-7">50</tspan></text>"#,
            r#"<text x="10" y="30" font-size="16">Units <tspan dx="0 -1 -0.5 0">1250</tspan></text>"#,
            // An element's own `dx` comes before the list around it.
            r#"<text x="10" y="30" font-size="16" dx="0 0 120 0">12<tspan dx="0">50</tspan></text>"#,
            // A label set along a path alone.
            r##"<text font-size="16"><textPath href="#p">Units 1250</textPath></text>"##,
        ] {
            assert!(!fuses(body), "{body}");
        }
    }

    #[test]
    fn flex_and_grid_items_stand_where_their_box_sets_them() {
        let fuses = |sheets: &[&str], body: &str| walk(sheets, body).fuses_blocks;
        for (sheets, body) in [
            // Flex items set apart in a column, by a gap, spread along the
            // line, or by a margin or padding, a reader's own among them;
            // grid items; and text straight inside, an item of its own. In
            // a row, digits that meet.
            (
                &[".s { display: flex; flex-direction: column }"][..],
                r#"<div class="s"><span>Balance due</span><span>1,250.00</span></div>"#.to_string(),
            ),
            (
                &[".s { display: grid }"],
                r#"<div class="s"><span>Item</span><span>1,250.00</span></div>"#.into(),
            ),
            (
                &[".s { display: flex; gap: .4em }"],
                r#"<div class="s">Balance due<span>1,250.00</span></div>"#.into(),
            ),
            (
                &[".s { display: flex; justify-content: space-between }"],
                r#"<div class="s"><span>Balance due</span><span>1,250.00</span></div>"#.into(),
            ),
            (
                &[".s { display: flex } .s > .l { margin: 0 1em 0 0 }"],
                r#"<div class="s"><span class="l">Balance due</span><span>1,250.00</span></div>"#
                    .into(),
            ),
            (
                &[],
                r#"<div style="display:flex">Balance due<span style="padding-left:6px">1,250.00</span></div>"#
                    .into(),
            ),
            (
                &["dl { display: flex }"],
                "<dl><dt>Basis</dt><dd>what you paid</dd></dl>".into(),
            ),
            (
                &[],
                r#"<div style="display:flex"><span>Units 12</span><span>50</span></div>"#.into(),
            ),
            // An inline box laying out items stands apart from the text
            // around it by its own padding, or its first or last item's,
            // and its last item in a column from the text after it; a box
            // without one of its own passes what it holds to its parent's
            // items.
            (
                &[".amt { display: inline-flex; padding: 0 .3em }"],
                r#"<p><span class="amt"><span>Total due</span></span>1,250.00</p>"#.into(),
            ),
            (
                &[".amt { display: inline-flex }"],
                r#"<p>Total due<span class="amt"><span style="padding-left:6px">1,250.00</span></span></p>"#
                    .into(),
            ),
            (
                &[".r { display: inline-flex; flex-direction: column }"],
                r#"<p>Rates <span class="r"><span>12</span> <span>15</span></span>and more</p>"#
                    .into(),
            ),
            (
                &[],
                r#"<div style="display:flex;flex-direction:column"><span style="display:contents"><b>Balance due</b><b>1,250.00</b></span></div>"#
                    .into(),
            ),
            (
                &[],
                r#"<div style="display:flex;flex-direction:column"><span style="display:contents"><b>Balance due</b></span><b>1,250.00</b></div>"#
                    .into(),
            ),
        ] {
            assert!(fuses(sheets, &body), "{body}");
        }
        for (sheets, body) in [
            // AnyDoc keeps white space between items.
            (
                &[".s { display: flex }"][..],
                r#"<div class="s"><span>Balance due</span> <span>1,250.00</span></div>"#
                    .to_string(),
            ),
            // Flex items in a row touch as a reader sets them, and may where
            // they may wrap; an inline box laying out items touches the text
            // around it, however it spreads them; an old flexible box in a
            // column, or clamping its lines, keeps its text in lines.
            (
                &[],
                r#"<div style="display:flex">Balance due<span>1,250.00</span></div>"#.into(),
            ),
            (
                &["h2.ct { display: flex; justify-content: center }"],
                r##"<h2 class="ct">The Long Winter<a href="#n1">1</a></h2>"##.into(),
            ),
            (
                &["p.term { display: flex; flex-wrap: wrap }"],
                r#"<p class="term">(<em>basis</em>) the amount paid.</p>"#.into(),
            ),
            (
                &[".amt { display: inline-flex; justify-content: space-between }"],
                r#"<p>Total due:<span class="amt"><span>$</span><span>1,250.00</span></span> by May.</p>"#
                    .into(),
            ),
            (
                &[".r { display: inline-flex; flex-direction: column }"],
                r#"<p>Rates <span class="r"><span>12</span></span>50 more</p>"#.into(),
            ),
            (
                &[".b { display: -webkit-box; -webkit-box-orient: vertical }"],
                r#"<div class="b"><span>Units 12</span><span>50</span></div>"#.into(),
            ),
            (
                &[".c { display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 3 }"],
                r##"<p class="c">The return lists income<a href="#n1">1</a> and credits.</p>"##
                    .into(),
            ),
        ] {
            assert!(!fuses(sheets, &body), "{body}");
        }
    }

    #[test]
    fn wrapping_flex_items_a_whole_line_wide_stand_apart() {
        let fuses = |sheets: &[&str], body: &str| walk(sheets, body).fuses_blocks;
        let rows = r#"<div class="r"><div class="l">Balance due</div><div class="a">Grand total</div></div>"#;
        // Where the items wrap, one whose width or basis takes the whole
        // line stands on a line of its own.
        for sheet in [
            ".r { display: flex; flex-wrap: wrap } .r > * { width: 100% }",
            ".r { display: flex; flex-flow: row wrap } .r > .a { flex-basis: 100% }",
            ".r { display: flex; flex-wrap: wrap } .r > .l { flex: 0 0 100% }",
            // A basis of auto takes the width: Bootstrap's row-cols-1.
            ".r { display: flex; flex-wrap: wrap } .r > * { width: 100% } .l, .a { flex: 1 0 0% } .r > * { flex: 0 0 auto }",
        ] {
            assert!(fuses(&[sheet], rows), "{sheet}");
        }
        // Items that fit on one line, or do not wrap, may touch; a basis
        // other than auto sets aside the width.
        for sheet in [
            ".r { display: flex; flex-wrap: wrap } .r > * { width: 50% }",
            ".r { display: flex } .r > * { width: 100% }",
            ".r { display: flex; flex-wrap: wrap } .r > * { width: 100%; flex: 1 0 0% }",
            ".r { display: flex; flex-wrap: wrap } .r > * { width: 30em }",
        ] {
            assert!(!fuses(&[sheet], rows), "{sheet}");
        }
        // An inline box whose items wrap so ends below its first line.
        assert!(fuses(
            &[".r { display: inline-flex; flex-wrap: wrap } .r > * { width: 100% }"],
            r#"<p>Due <span class="r"><span>Balance</span><span>1,250.00</span></span>Grand total</p>"#
        ));
    }

    #[test]
    fn spacing_takes_its_size_from_the_custom_properties_it_names() {
        let fuses = |sheets: &[&str], body: &str| walk(sheets, body).fuses_blocks;
        // A grid's gutter, as Bootstrap 5 sets it: a custom property on the
        // row that its columns' padding takes half of.
        let grid = ".row { --gutter: 1.5rem; display: flex; flex-wrap: wrap; margin-left: calc(-.5 * var(--gutter)) } .row > * { width: 100%; padding-left: calc(var(--gutter) * .5); padding-right: calc(var(--gutter) * .5) } .col { flex: 1 0 0% } .g-0 { --gutter: 0 }";
        let row = |class: &str, style: &str| {
            format!(
                r#"<div class="{class}" style="{style}"><div class="col">Opening balance</div><div class="col">Closing balance</div></div>"#
            )
        };
        assert!(fuses(&[grid], &row("row", "")));
        // No gutter, from a class or an inline style, and one that may not
        // hold, set nothing apart for certain.
        assert!(!fuses(&[grid], &row("row g-0", "")));
        assert!(!fuses(&[grid], &row("row", "--gutter: 0")));
        assert!(!fuses(
            &[grid, "@media (min-width: 40em) { .row { --gutter: 0 } }"],
            &row("row", "")
        ));
        // A fallback where nothing sets the property, and a gap.
        let rows = r#"<div class="r"><span>Balance due</span><span>Grand total</span></div>"#;
        assert!(fuses(
            &[".r { display: flex } .r > * { padding-left: var(--pad, 6px) }"],
            rows
        ));
        assert!(!fuses(
            &[".r { display: flex } .r > * { padding-left: var(--pad) }"],
            rows
        ));
        assert!(fuses(
            &[".r { display: flex; --g: 1em; gap: var(--g) }"],
            rows
        ));
        assert!(!fuses(
            &[".r { display: flex; --g: 0; gap: var(--g) }"],
            rows
        ));
    }

    #[test]
    fn custom_properties_are_settled_once_at_each_element() {
        // Long inline styles on the ancestors are read once each, however
        // many elements below take their spacing from a custom property.
        let style: String = (0..2_000).map(|at| format!("--p{at}: {at}px;")).collect();
        let body = format!(
            r#"{}<p>{}</p><p style="display:flex">{}</p>{}"#,
            format!(r#"<div style="{style}">"#).repeat(20),
            "<span>w</span>".repeat(2_000),
            "<span>w</span>".repeat(2_000),
            "</div>".repeat(20)
        );
        let (reader, anydoc) = cascade_for(&["span { margin-left: var(--gap) }"]);
        let started = std::time::Instant::now();
        let mut work = 0;
        chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("chapter walk");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert!(work < 1_000_000, "{work}");
        // Rules setting one property for many classes, as Bootstrap's
        // gutters are set, are tried only at the elements carrying each.
        let sheet: String = (0..400)
            .map(|at| format!(".g{at} {{ --gap: {at}px }}\n"))
            .chain([".r { display: flex } .r > * { padding-left: var(--gap) }".to_string()])
            .collect();
        let body = r#"<div class="r g3"><span>Balance due</span><span>Grand total</span></div>"#
            .repeat(500);
        let (reader, anydoc) = cascade_for(&[&sheet]);
        let mut work = 0;
        let found = chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("walk");
        assert!(found.fuses_blocks);
        assert!(work < 100_000, "{work}");
    }

    #[test]
    fn attribute_values_compare_without_case_as_lowercase_does() {
        let values = [
            "",
            "a",
            "A",
            "note",
            "Note",
            "NOTE",
            "noteref",
            "footnote NoteRef",
            "en-US",
            "EN",
            "en",
            "z3998:Roman",
            "a\u{b}b",
            "a b",
            "\u{c4}rger",
            "\u{e4}RGER",
            "-",
            "x-",
        ];
        let operators = [
            AttributeOperator::Exists,
            AttributeOperator::Equals,
            AttributeOperator::Includes,
            AttributeOperator::DashMatch,
            AttributeOperator::Prefix,
            AttributeOperator::Suffix,
            AttributeOperator::Substring,
        ];
        for operator in operators {
            for actual in values {
                for wanted in values {
                    assert_eq!(
                        attribute_test_folded(operator, actual, wanted),
                        attribute_test(operator, &actual.to_lowercase(), &wanted.to_lowercase()),
                        "{operator:?} {actual:?} {wanted:?}"
                    );
                }
            }
        }
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
            // Currency signs of every script, and signs in their full-width
            // forms.
            (
                r#".s::before { content: "\E3F" }"#,
                r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#,
            ),
            (
                r#".s::before { content: "\58F" }"#,
                r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#,
            ),
            (
                r#".s::before { content: "\FFE5" }"#,
                r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#,
            ),
            (
                r#".s::before { content: "\FF0D" }"#,
                r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#,
            ),
            (
                r#".s::after { content: "\2030" }"#,
                r#"<p>Rate <span class="s">12</span> this year.</p>"#,
            ),
            (
                r#".s::after { content: "\66A" }"#,
                r#"<p>Rate <span class="s">12</span> this year.</p>"#,
            ),
            // A sign before an amount that opens with a currency sign or a
            // decimal point, and after one that ends with a percent sign.
            (
                r#".s::before { content: "\2212" }"#,
                r#"<p>Net change <span class="s">$1,250.00</span> this year.</p>"#,
            ),
            (
                r#".s::before { content: "\2212" }"#,
                r#"<p>Rate change <span class="s">.75</span> points.</p>"#,
            ),
            (
                r#".s::before { content: "-" }"#,
                r#"<p>Net change <span class="s">€1.250,00</span> this year.</p>"#,
            ),
            (
                r#".s::after { content: ")" }"#,
                r#"<p>Change <span class="s">12%</span> this year.</p>"#,
            ),
            // Anything a box shows between digits: a raised or Arabic
            // decimal point, a slash, a colon, a comma and a space between
            // page references, a space, or a quote mark.
            (
                r#".c::before { content: "\B7" }"#,
                r#"<p>Price 1<span class="c">99</span> due.</p>"#,
            ),
            (
                r#".c::before { content: "\66B" }"#,
                r#"<p>Price 1<span class="c">99</span> due.</p>"#,
            ),
            (
                r#".c::before { content: "\2044" }"#,
                r#"<p>Take 1<span class="c">2</span> cup.</p>"#,
            ),
            (
                r#".c::before { content: ":" }"#,
                r#"<p>At 12<span class="c">30</span> sharp.</p>"#,
            ),
            (
                r#".pg + .pg::before { content: ", " }"#,
                r##"<p>Deductions, <a href="#p12" class="pg">12</a><a href="#p15" class="pg">15</a></p>"##,
            ),
            (
                r#".c::before { content: " " }"#,
                r#"<p>Price 1<span class="c">99</span> due.</p>"#,
            ),
            (
                r#"q::before { content: open-quote }"#,
                "<p>In 2023<q>15 cases</q> closed.</p>",
            ),
            // Characters a reader shows nothing for, between a sign and its
            // digits.
            (
                r#".s::before { content: "\2212" }"#,
                "<p>Net change <span class=\"s\">\u{200e}1,250.00</span> this year.</p>",
            ),
            (
                r#".c::before { content: "." }"#,
                "<p>Price 1<span class=\"c\">\u{2060}99</span> due.</p>",
            ),
            (
                r#".s::after { content: "%" }"#,
                "<p>Rate <span class=\"s\">12\u{200e}</span> this year.</p>",
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
            // A list item's bullet, a hyphen or dash set apart from its
            // text: AnyDoc's list marker stands in for it.
            (
                r#"li::before { content: "- " }"#,
                "<ul><li>2 cups flour</li></ul>",
            ),
            (
                r#"ul.b { list-style: none } ul.b li::before { content: "\2013\A0" }"#,
                r#"<ul class="b"><li>2 cups flour</li><li>3 eggs</li></ul>"#,
            ),
            (
                r#"li { display: inline } li + li::before { content: " - " }"#,
                "<ul><li>2024</li><li>2025</li></ul>",
            ),
            // What a box shows apart from digits on one side, or beside
            // characters a reader shows nothing for.
            (
                r#".pg + .pg::before { content: ", " }"#,
                r##"<p>See <a href="#a" class="pg">Filing</a><a href="#b" class="pg">12</a></p>"##,
            ),
            (
                r#".c::before { content: "/" }"#,
                r#"<p>Price 1<span class="c"> each</span></p>"#,
            ),
            (
                r#".c::before { content: "\200B" }"#,
                r#"<p>Price 1<span class="c">99</span> due.</p>"#,
            ),
            (
                r#".c::before { content: url("rule.png") }"#,
                r#"<p>Chapter <span class="c">One</span></p>"#,
            ),
        ] {
            assert!(!drops_shown(&[sheets], body), "{sheets} {body}");
        }
    }

    #[test]
    fn conditions_hold_as_far_as_the_check_can_tell() {
        let media = |text: &str| media_condition(&tokenize(text).expect("tokens"));
        for (query, holds) in [
            ("", Applies::Yes),
            ("screen", Applies::Yes),
            ("only screen", Applies::Yes),
            ("all, print", Applies::Yes),
            ("not print", Applies::Yes),
            ("print", Applies::No),
            ("amzn-kf8", Applies::No),
            ("not screen", Applies::No),
            ("print and (color)", Applies::No),
            // Features every reader the check follows has, or none has.
            ("(min-width: 0)", Applies::Yes),
            ("screen and (min-width: 1px)", Applies::Yes),
            ("(width >= 0) and (color)", Applies::Yes),
            ("not all and (monochrome)", Applies::Yes),
            ("(min-resolution: 1dppx)", Applies::Yes),
            ("(max-width: 1px)", Applies::No),
            ("(min-width: 0) and (max-width: 1px)", Applies::No),
            ("(max-resolution: 0.5dppx)", Applies::No),
            ("(grid)", Applies::No),
            // Features that set readers apart.
            ("screen and (min-width: 600px)", Applies::Varies),
            ("(max-width: 40em)", Applies::Varies),
            ("(400px <= width <= 700px)", Applies::Varies),
            ("(orientation: portrait)", Applies::Varies),
            ("(min-aspect-ratio: 16/9)", Applies::Varies),
            ("(min-resolution: 2dppx)", Applies::Varies),
            ("(hover: hover)", Applies::Varies),
            ("(prefers-color-scheme: dark)", Applies::Varies),
            // A query Chromium rejects holds nowhere, and a feature it does
            // not know fails.
            ("screen screen", Applies::No),
            ("not only print", Applies::No),
            (", print", Applies::No),
            ("(min-width)", Applies::No),
            ("(bogus-feature)", Applies::No),
            ("not (bogus-feature)", Applies::No),
            ("(-ms-high-contrast: none)", Applies::No),
            ("(min-width: 0) or (bogus)", Applies::Yes),
            // Lengths the check cannot size.
            ("(min-width: calc(100px + 1em))", Applies::Doubt),
            ("(max-width: 50vw)", Applies::Doubt),
        ] {
            assert_eq!(media(query), holds, "@media {query}");
        }
        let supports = |text: &str| supports_condition(&tokenize(text).expect("tokens"));
        for (condition, holds) in [
            ("(display: grid)", Applies::Yes),
            ("((display: flex))", Applies::Yes),
            ("(display: inline flow-root)", Applies::Yes),
            ("(display: bogus-value)", Applies::No),
            ("(display: -moz-box)", Applies::No),
            ("(display: run-in)", Applies::No),
            ("not (display: bogus-value)", Applies::Yes),
            ("not (display: grid)", Applies::No),
            ("(display: grid) and (display: bogus)", Applies::No),
            ("(display: grid) or (display: bogus)", Applies::Yes),
            // Properties Chromium knows, whatever their value, and those it
            // does not.
            ("(display: flex) and (gap: 1em)", Applies::Yes),
            ("(position: sticky)", Applies::Yes),
            ("(--anything: 1)", Applies::Yes),
            ("(-webkit-hyphens: none)", Applies::No),
            ("(margin-trim: inline)", Applies::No),
            ("(-moz-orient: inline)", Applies::No),
            // Selectors it parses, and those it rejects.
            ("selector(:has(a))", Applies::Yes),
            ("selector(p > q::before)", Applies::Yes),
            ("selector(:-moz-focusring)", Applies::No),
            ("selector(p, q)", Applies::No),
            ("font-tech(color-COLRv1)", Applies::Doubt),
            ("bogus-function(x)", Applies::No),
            // `not` must stand in parentheses beside `and`: the rule is void.
            ("(display: grid) and not (display: bogus)", Applies::No),
        ] {
            assert_eq!(supports(condition), holds, "@supports {condition}");
        }
        // Parentheses nested deep are not followed far.
        let deep = format!(
            "{}(display: grid){}",
            "(".repeat(100_000),
            ")".repeat(100_000)
        );
        assert_eq!(supports(&deep), Applies::Doubt);
        let deep = format!("{}(color){}", "(".repeat(100_000), ")".repeat(100_000));
        assert_eq!(media(&deep), Applies::Doubt);
    }

    #[test]
    fn rules_whose_conditions_hold_for_every_reader_apply() {
        let body = r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#;
        // A sign every reader the check follows shows.
        for guarded in [
            r#"@media (min-width: 0) { .s::before { content: "\2212" } }"#,
            r#"@media screen and (min-width: 1px) { .s::before { content: "\2212" } }"#,
            r#"@supports (gap: 1px) { .s::before { content: "\2212" } }"#,
            r#"@supports selector(:has(p)) { .s::before { content: "\2212" } }"#,
            r#"@scope (body) { .s::before { content: "\2212" } }"#,
            r#"@scope (p) { :scope > .s::before { content: "\2212" } }"#,
        ] {
            assert!(drops_shown(&[guarded], body), "{guarded}");
        }
        // Not at a scope's root itself, nor past its limits; nor in a
        // container query where no container stands around the element,
        // nor where Chromium rejects the condition.
        for guarded in [
            r#"@scope (.s) { .s::before { content: "\2212" } }"#,
            r#"@scope (p) to (.s) { .s::before { content: "\2212" } }"#,
            r#"@container (min-width: 0) { .s::before { content: "\2212" } }"#,
            r#"@media screen screen { .s::before { content: "\2212" } }"#,
            r#"@supports (display: -moz-box) { .s::before { content: "\2212" } }"#,
        ] {
            assert!(!drops_shown(&[guarded], body), "{guarded}");
        }
        // A rule that holds everywhere shows what AnyDoc hides, and keeps
        // hidden what it converts; one that holds nowhere hides nothing.
        let refund = r#"<p>Refund due <span class="x">1,250.00</span> by April.</p>"#;
        for sheet in [
            ".x { display: none } @media (min-width: 0) { .x { display: inline } }",
            ".x { display: none } @supports (gap: 1px) { .x { display: inline } }",
            "@media (max-width: 1px) { .d { color: red } .x { display: none } }",
            "@media not only print { .d { color: red } .x { display: none } }",
            "@container (min-width: 0) { .d { color: red } .x { display: none } }",
            "@supports (display: -moz-box) { .d { color: red } .x { display: none } }",
        ] {
            assert!(drops_shown(&[sheet], refund), "{sheet}");
        }
        assert!(!converts_hidden(
            &["p .x { display: none } @media (min-width: 0) { p span.x { display: inline } }"],
            refund
        ));
        // A rule nested nine deep matches as one nested once does.
        let nested = format!(
            ".x {{ display: none }} {}.x {{ display: inline }}{}",
            ".a { ".repeat(9),
            " }".repeat(9)
        );
        let deep = format!(
            "{}<p>Refund due <span class=\"x\">1,250.00</span> by April.</p>{}",
            r#"<div class="a">"#.repeat(10),
            "</div>".repeat(10)
        );
        assert!(drops_shown(&[&nested], &deep));
    }

    #[test]
    fn rules_whose_conditions_may_not_hold_neither_hide_nor_show() {
        let body = r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#;
        let sign = r#".s::before { content: "\2212" }"#;
        // A rule a condition guards that may not hold, or never holds,
        // keeps no sign from showing.
        for guarded in [
            "@supports (display: bogus-value) { .s::before { opacity: 0 } }",
            "@supports not (display: grid) { .s::before { opacity: 0 } }",
            "@media (min-width: 5000px) { .s::before { visibility: hidden } }",
            "@media print { .s::before { visibility: hidden } }",
            "@container (min-width: 5000px) { .s::before { opacity: 0 } }",
            "@scope (.nothing) { .s::before { opacity: 0 } }",
            "@starting-style { .s::before { opacity: 0 } }",
            "@-moz-document url-prefix() { .s::before { opacity: 0 } }",
        ] {
            assert!(drops_shown(&[sign, guarded], body), "{guarded}");
        }
        assert!(!drops_shown(
            &[
                sign,
                "@supports (display: grid) { .s::before { opacity: 0 } }"
            ],
            body
        ));
        // Nor shows a sign it gives.
        for guarded in [
            r#"@supports (display: totally-bogus) { .s::before { content: "\2212" } }"#,
            r#"@media (min-width: 40em) { .s::before { content: "\2212" } }"#,
            r#"@starting-style { .s::before { content: "\2212" } }"#,
        ] {
            assert!(!drops_shown(&[guarded], body), "{guarded}");
        }
        assert!(drops_shown(
            &[r#"@media screen { .s::before { content: "\2212" } }"#],
            body
        ));
        // Blocks a flex box that may not be one lays out stand apart.
        let rows = r#"<div class="r"><div>Balance due</div><div>Grand total</div></div>"#;
        for guarded in [
            "@media (min-width: 900px) { .r { display: flex } }",
            "@supports (display: bogus-value) { .r { display: flex } }",
            "@-moz-document url-prefix() { .r { display: flex } }",
            "@starting-style { .r { display: flex } }",
        ] {
            assert!(walk(&[guarded], rows).fuses_blocks, "{guarded}");
        }
        assert!(!walk(&[".r { display: flex }"], rows).fuses_blocks);
    }

    #[test]
    fn nested_rules_match_inside_the_rule_they_are_nested_in() {
        let body = r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#;
        let shown = |sheet: &str| drops_shown(&[sheet], body);
        // `&` stands for what the rule nested in matches, and a selector
        // without one matches inside that rule's elements.
        for sheet in [
            r#".s { &::before { content: "\2212" } }"#,
            r#"p { .s::before { content: "\2212" } }"#,
            r#"body { > p { > .s::before { content: "\2212" } } }"#,
            r#".a { .s:not(&)::before { content: "\2212" } }"#,
            r#"p { @media screen { .s::before { content: "\2212" } } }"#,
            r#"body { p { .s { &::before { content: "\2212" } } } }"#,
            // As much weight as the most specific selector it stands for.
            r#"#i, p { & .s::before { content: "\2212" } } p span.s::before { content: none }"#,
        ] {
            assert!(shown(sheet), "{sheet}");
        }
        for sheet in [
            r#".wrap { color: red; & .s::before { content: "\2212" } }"#,
            r#".zz { &::before { content: "\2212" } }"#,
            r#"div { .s::before { content: "\2212" } }"#,
            r#"body { > .s::before { content: "\2212" } }"#,
            // A pseudo-element's rule matches no element for `&`.
            r#"p::before { & .s::before { content: "\2212" } }"#,
        ] {
            assert!(!shown(sheet), "{sheet}");
        }
        // Declarations after a nested rule come after it in the cascade.
        assert!(shown(
            r#".s::before { content: "\2212"; opacity: 0; @media screen { opacity: 1 } }"#
        ));
        assert!(!shown(
            r#".s::before { content: "\2212"; @media screen { opacity: 1 } opacity: 0 }"#
        ));
        // Outside a rule, `&` is the root.
        let blocks = r#"<p>Balance due<span class="s">Grand total</span></p>"#;
        assert!(walk(&["p { & .s { display: block } }"], blocks).fuses_blocks);
        assert!(walk(&["& .s { display: block }"], blocks).fuses_blocks);
        assert!(!walk(&[".a { .s { display: block } }"], blocks).fuses_blocks);
    }

    #[test]
    fn nested_rules_share_their_parents_selectors_within_bounds() {
        // Rules nested in one share its parsed selectors.
        let parent: Vec<String> = (0..200).map(|at| format!(".p{at}")).collect();
        let css = format!(
            "{} {{ {} }}",
            parent.join(", "),
            ".s { display: block } ".repeat(2_000)
        );
        let sheet = parse_stylesheet(&css).expect("sheet");
        assert_eq!(sheet.rules.len(), 2_000);
        let shared = |rule: &StyleRule| match &rule.selector.compounds[0].parts[..] {
            [Simple::Pseudo(PseudoClass::Is(list))] => list.clone(),
            parts => panic!("{parts:?}"),
        };
        let first = shared(&sheet.rules[0]);
        assert_eq!(first.len(), 200);
        assert!(sheet
            .rules
            .iter()
            .all(|rule| Rc::ptr_eq(&shared(rule), &first)));
        // `&`s taken twice at every level would stand for 8^12 compound
        // selectors, which the work allowed would run out walking. Past the
        // bounds, a selector is undecided: it may match, which shows no
        // sign for certain, and the walk stays well within the work allowed.
        let css = format!(
            ".a, .b, .c, .d {{ {} .s::before {{ content: \"\\2212\" }} {} }}",
            "display: block; & &, & &, & &, & & { ".repeat(12),
            "} ".repeat(12)
        );
        let sheet = parse_stylesheet(&css).expect("sheet");
        assert!(sheet
            .rules
            .iter()
            .all(|rule| rule.selector.weight <= MAX_SELECTOR_WEIGHT));
        let (reader, anydoc) = cascade_for(&[css.as_str()]);
        let body = format!(
            r#"{}<p>Net change <span class="s">1,250.00</span> this year.</p>{}"#,
            r#"<div class="a">"#.repeat(40),
            "</div>".repeat(40)
        );
        let mut work = 0;
        let found = chapter_text(&chapter(&body), &reader, &anydoc, &mut work).expect("walk");
        assert!(!found.drops_shown);
        assert!(work < MAX_MATCH_WORK / 10, "{work}");
    }

    #[test]
    fn cascade_layers_order_the_rules_in_them() {
        let body = r#"<p>Net change <span class="s">1,250.00</span> this year.</p>"#;
        let shown = |sheet: &str| drops_shown(&[sheet], body);
        // Unlayered rules beat layered ones, whatever their specificity, and
        // a later layer an earlier one; for `!important`, the reverse.
        assert!(shown(
            r#"@layer l { p .s::before { opacity: 0 } } .s::before { content: "\2212"; opacity: 1 }"#
        ));
        assert!(!shown(
            r#"@layer l { .s::before { opacity: 0 !important } } .s::before { content: "\2212"; opacity: 1 !important }"#
        ));
        assert!(!shown(
            r#"@layer a, b; @layer b { .s::before { opacity: 0 } } @layer a { p .s::before { opacity: 1 } } .s::before { content: "\2212" }"#
        ));
        assert!(shown(
            r#"@layer b, a; @layer b { .s::before { opacity: 0 } } @layer a { p .s::before { opacity: 1 } } .s::before { content: "\2212" }"#
        ));
        // A layer's own rules beat its sublayers', and layers of one name
        // across sheets are one.
        assert!(shown(
            r#"@layer a { .s::before { opacity: 1 } @layer b { p .s::before { opacity: 0 } } } .s::before { content: "\2212" }"#
        ));
        assert!(drops_shown(
            &[
                r#"@layer x, y; @layer y { .s::before { content: "\2212" } }"#,
                "@layer x { p .s::before { content: none } }",
            ],
            body
        ));
        let rows = r#"<div class="r"><div>Balance due</div><div>Grand total</div></div>"#;
        assert!(
            walk(
                &["@layer l { body .r { display: flex } } .r { display: block }"],
                rows
            )
            .fuses_blocks
        );
        assert!(
            !walk(
                &["@layer l { .r { display: block } } body .r { display: flex }"],
                rows
            )
            .fuses_blocks
        );
    }

    #[test]
    fn a_list_items_signs_are_lost_as_elsewhere() {
        // AnyDoc's list marker stands in for a list item's bullet, not for
        // a sign it shows before or after its digits: a minus, parentheses,
        // a percent or currency sign, or a hyphen or dash touching them.
        let items = r#"<ul class="ledger"><li>Opening balance</li><li class="neg">1,250.00</li><li class="pct">12</li></ul>"#;
        for sheet in [
            r#".neg::before { content: "\2212" }"#,
            r#".neg::before { content: "(" } .neg::after { content: ")" }"#,
            r#".pct::after { content: "%" }"#,
            r#".neg::before { content: "$" }"#,
            r#".neg::before { content: "-" }"#,
            r#"ul.ledger { list-style: none } .neg::before { content: "\2212" }"#,
            r#"ul.ledger { list-style: none } li::before { content: "\2013 " }"#,
        ] {
            assert!(drops_shown(&[sheet], items), "{sheet}");
            let numbered = items.replace("ul", "ol");
            assert!(drops_shown(&[sheet], &numbered), "{sheet}");
        }
        // A hyphen or dash a margin, padding, or white space sets apart
        // from the item's text is its bullet; one set apart after its
        // digits, a separator from the next item, which starts a line of
        // its own in the Markdown.
        for sheet in [
            r#"ul.ledger { list-style: none } li::before { content: "-"; margin-right: .5em }"#,
            r#"ul.ledger { list-style: none } li::before { content: "\2013"; padding-right: 4px }"#,
            r#"ul.ledger { list-style: none } li::before { content: "-"; position: absolute; margin-left: -1em }"#,
            r#"ul.ledger { list-style: none } li::before { content: "\2013\A0" }"#,
            r#"li { display: inline } li:not(:last-child)::after { content: " - " }"#,
            r#"li { display: inline } li + li::before { content: " \B7 " }"#,
        ] {
            assert!(!drops_shown(&[sheet], items), "{sheet}");
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

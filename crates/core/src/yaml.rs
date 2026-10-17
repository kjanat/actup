//! Loads one YAML document under the YAML 1.2 core schema that the GitHub Actions runner reads.

mod simple_upper;

use std::{collections::HashMap, collections::HashSet, fmt};

use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};
use serde::de::{
    self, DeserializeOwned, Deserializer, IntoDeserializer, Visitor,
    value::{MapDeserializer, SeqDeserializer},
};

/// The most nodes that alias expansion may add to one document.
pub const MAX_EXPANDED_NODES: usize = 10_000;

/// Which reader's rules the loader applies beyond the shared scalar rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// actup config files: aliases expand up to [`MAX_EXPANDED_NODES`] nodes, keys compare exactly and collection tags must be core tags.
    Config,
    /// Files the actions runner reads: anchors are rejected, keys compare as .NET `OrdinalIgnoreCase` and collection tags are ignored.
    Runner,
}

impl Dialect {
    /// The most sequences and mappings that may nest inside each other.
    #[must_use]
    pub const fn max_depth(self) -> usize {
        match self {
            Self::Config => 128,
            Self::Runner => 100,
        }
    }
}

/// A YAML load or deserialization error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
    mark: Option<Mark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    line: usize,
    column: usize,
}

impl Mark {
    fn of(span: Span) -> Self {
        Self {
            line: span.start.line(),
            column: span.start.col() + 1,
        }
    }
}

impl Error {
    fn at(message: impl Into<String>, mark: Mark) -> Self {
        Self {
            message: message.into(),
            mark: Some(mark),
        }
    }

    fn located(mut self, mark: Mark) -> Self {
        self.mark.get_or_insert(mark);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.mark {
            Some(Mark { line, column }) => {
                write!(f, "{} at line {line}, column {column}", self.message)
            }
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self {
            message: msg.to_string(),
            mark: None,
        }
    }
}

/// Deserialize `T` from the single YAML document in `text`.
///
/// A text without a document reads as an empty mapping.
///
/// # Errors
///
/// Returns [`Error`] for invalid YAML, a `<<` merge key, a duplicate or
/// non-scalar mapping key, a tag outside the core schema, a second document,
/// a tag on a quoted or block scalar other than `!!str`, a `0x` or `0o`
/// integer wider than 32 bits, nesting past [`Dialect::max_depth`], an
/// anchor, key or tag that `dialect` rejects, alias expansion past
/// [`MAX_EXPANDED_NODES`], or a document that does not fit `T`.
pub fn from_str<T: DeserializeOwned>(text: &str, dialect: Dialect) -> Result<T, Error> {
    T::deserialize(&load(text, dialect)?)
}

#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    mark: Mark,
    size: usize,
    height: usize,
}

#[derive(Debug, Clone)]
enum Kind {
    Scalar {
        text: String,
        value: Scalar,
        plain: bool,
    },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Scalar {
    Null,
    Bool(bool),
    Unsigned(u64),
    Signed(i64),
    Float(f64),
    Str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreTag {
    Str,
    Null,
    Bool,
    Int,
    Float,
    Seq,
    Map,
}

enum Frame {
    Seq {
        mark: Mark,
        anchor: usize,
        items: Vec<Node>,
    },
    Map {
        mark: Mark,
        anchor: usize,
        entries: Vec<(Node, Node)>,
        key: Option<Node>,
        seen: HashSet<String>,
    },
}

struct Loader {
    dialect: Dialect,
    defined: HashMap<usize, Node>,
    expanded: usize,
    stack: Vec<Frame>,
    root: Option<Node>,
}

fn load(text: &str, dialect: Dialect) -> Result<Node, Error> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut loader = Loader {
        dialect,
        defined: HashMap::new(),
        expanded: 0,
        stack: Vec::new(),
        root: None,
    };
    let mut documents = 0_usize;
    for event in Parser::new_from_str(body) {
        let (event, span) = event.map_err(|e| {
            let mark = e.marker();
            Error::at(
                e.info(),
                Mark {
                    line: mark.line(),
                    column: mark.col() + 1,
                },
            )
        })?;
        let mark = Mark::of(span);
        match event {
            Event::DocumentStart(_) => {
                documents += 1;
                if documents > 1 {
                    return Err(Error::at("a file holds more than one YAML document", mark));
                }
            }
            Event::Scalar(text, style, anchor, tag) => {
                let node = scalar(text.into_owned(), style, tag.as_deref(), mark)?;
                loader.define(anchor, &node)?;
                loader.push(node)?;
            }
            Event::SequenceStart(anchor, tag) => {
                loader.expect_tag(tag.as_deref(), CoreTag::Seq, mark)?;
                loader.open(anchor, mark)?;
                loader.stack.push(Frame::Seq {
                    mark,
                    anchor,
                    items: Vec::new(),
                });
            }
            Event::MappingStart(anchor, tag) => {
                loader.expect_tag(tag.as_deref(), CoreTag::Map, mark)?;
                loader.open(anchor, mark)?;
                loader.stack.push(Frame::Map {
                    mark,
                    anchor,
                    entries: Vec::new(),
                    key: None,
                    seen: HashSet::new(),
                });
            }
            Event::SequenceEnd | Event::MappingEnd => loader.close()?,
            Event::Alias(id) => loader.alias(id, mark)?,
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
        }
    }
    Ok(loader.root.unwrap_or(Node {
        kind: Kind::Map(Vec::new()),
        mark: Mark { line: 1, column: 1 },
        size: 1,
        height: 1,
    }))
}

impl Loader {
    fn check_anchor(&self, anchor: usize, mark: Mark) -> Result<(), Error> {
        if anchor != 0 && self.dialect == Dialect::Runner {
            return Err(Error::at("anchors are not supported", mark));
        }
        Ok(())
    }

    fn check_depth(&self, height: usize, mark: Mark) -> Result<(), Error> {
        let max = self.dialect.max_depth();
        if self.stack.len().saturating_add(height) > max {
            return Err(Error::at(
                format!("sequences and mappings nest deeper than {max} levels"),
                mark,
            ));
        }
        Ok(())
    }

    fn expect_tag(&self, tag: Option<&Tag>, kind: CoreTag, mark: Mark) -> Result<(), Error> {
        expect_tag(self.dialect, tag, kind, mark)
    }

    fn open(&self, anchor: usize, mark: Mark) -> Result<(), Error> {
        self.check_anchor(anchor, mark)?;
        self.check_depth(1, mark)
    }

    fn define(&mut self, anchor: usize, node: &Node) -> Result<(), Error> {
        self.check_anchor(anchor, node.mark)?;
        if anchor != 0 {
            self.defined.insert(anchor, node.clone());
        }
        Ok(())
    }

    fn alias(&mut self, id: usize, mark: Mark) -> Result<(), Error> {
        let Some(target) = self.defined.get(&id) else {
            return Err(Error::at(
                "alias names an anchor whose node is not complete",
                mark,
            ));
        };
        self.check_depth(target.height, mark)?;
        self.expanded = self.expanded.saturating_add(target.size);
        if self.expanded > MAX_EXPANDED_NODES {
            return Err(Error::at(
                format!("aliases expand to more than {MAX_EXPANDED_NODES} nodes"),
                mark,
            ));
        }
        let mut node = target.clone();
        node.mark = mark;
        self.push(node)
    }

    fn close(&mut self) -> Result<(), Error> {
        let (node, anchor) = match self.stack.pop() {
            Some(Frame::Seq {
                mark,
                anchor,
                items,
            }) => {
                let size = items.iter().map(|n| n.size).fold(1, usize::saturating_add);
                let height = 1 + items.iter().map(|n| n.height).max().unwrap_or(0);
                (
                    Node {
                        kind: Kind::Seq(items),
                        mark,
                        size,
                        height,
                    },
                    anchor,
                )
            }
            Some(Frame::Map {
                mark,
                anchor,
                entries,
                ..
            }) => {
                let size = entries
                    .iter()
                    .map(|(k, v)| k.size.saturating_add(v.size))
                    .fold(1, usize::saturating_add);
                let height = 1 + entries
                    .iter()
                    .map(|(k, v)| k.height.max(v.height))
                    .max()
                    .unwrap_or(0);
                (
                    Node {
                        kind: Kind::Map(entries),
                        mark,
                        size,
                        height,
                    },
                    anchor,
                )
            }
            None => return Ok(()),
        };
        self.define(anchor, &node)?;
        self.push(node)
    }

    fn push(&mut self, node: Node) -> Result<(), Error> {
        let dialect = self.dialect;
        match self.stack.last_mut() {
            None => self.root = Some(node),
            Some(Frame::Seq { items, .. }) => items.push(node),
            Some(Frame::Map {
                entries, key, seen, ..
            }) => {
                if let Some(k) = key.take() {
                    entries.push((k, node));
                    return Ok(());
                }
                let Kind::Scalar { text, plain, .. } = &node.kind else {
                    return Err(Error::at("a mapping key must be a scalar", node.mark));
                };
                if *plain && text == "<<" {
                    return Err(Error::at("merge keys are not supported", node.mark));
                }
                let folded = match dialect {
                    Dialect::Config => Some(text.clone()),
                    Dialect::Runner if text.contains("${{") => None,
                    Dialect::Runner => Some(fold_case(text)),
                };
                if folded.is_some_and(|folded| !seen.insert(folded)) {
                    return Err(Error::at(format!("duplicate key `{text}`"), node.mark));
                }
                *key = Some(node);
            }
        }
        Ok(())
    }
}

fn core_tag(tag: &Tag, mark: Mark) -> Result<CoreTag, Error> {
    let known = if tag.is_yaml_core_schema() {
        match tag.suffix.as_str() {
            "str" => Some(CoreTag::Str),
            "null" => Some(CoreTag::Null),
            "bool" => Some(CoreTag::Bool),
            "int" => Some(CoreTag::Int),
            "float" => Some(CoreTag::Float),
            "seq" => Some(CoreTag::Seq),
            "map" => Some(CoreTag::Map),
            _ => None,
        }
    } else {
        None
    };
    known.ok_or_else(|| {
        Error::at(
            format!("tag `{}` is not a core schema tag", tag_name(tag)),
            mark,
        )
    })
}

fn tag_name(tag: &Tag) -> String {
    if tag.is_yaml_core_schema() {
        format!("!!{}", tag.suffix)
    } else {
        tag.to_string()
    }
}

fn expect_tag(dialect: Dialect, tag: Option<&Tag>, kind: CoreTag, mark: Mark) -> Result<(), Error> {
    if dialect == Dialect::Runner {
        return Ok(());
    }
    match tag.map(|t| core_tag(t, mark)).transpose()? {
        Some(found) if found != kind => Err(Error::at(
            format!(
                "tag `{}` does not fit this node",
                tag.map_or_else(String::new, tag_name)
            ),
            mark,
        )),
        _ => Ok(()),
    }
}

fn scalar(text: String, style: ScalarStyle, tag: Option<&Tag>, mark: Mark) -> Result<Node, Error> {
    let plain = style == ScalarStyle::Plain;
    let resolved = |text: &str| resolve(text).map_err(|message| Error::at(message, mark));
    let value = match tag.map(|t| core_tag(t, mark)).transpose()? {
        None if plain => resolved(&text)?,
        None | Some(CoreTag::Str) => Scalar::Str,
        Some(_) if !plain => {
            let tag = tag.map_or_else(String::new, tag_name);
            return Err(Error::at(format!("tag `{tag}` needs a plain scalar"), mark));
        }
        Some(expected) => {
            let value = resolved(&text)?;
            let typed = match expected {
                CoreTag::Null => (value == Scalar::Null).then_some(value),
                CoreTag::Bool => matches!(value, Scalar::Bool(_)).then_some(value),
                CoreTag::Int => {
                    matches!(value, Scalar::Unsigned(_) | Scalar::Signed(_)).then_some(value)
                }
                CoreTag::Float => match value {
                    Scalar::Float(_) => Some(value),
                    _ if is_float(&text) => text.parse().ok().map(Scalar::Float),
                    _ => None,
                },
                CoreTag::Str | CoreTag::Seq | CoreTag::Map => None,
            };
            let Some(value) = typed else {
                let tag = tag.map_or_else(String::new, tag_name);
                return Err(Error::at(
                    format!("`{text}` does not fit tag `{tag}`"),
                    mark,
                ));
            };
            value
        }
    };
    Ok(Node {
        kind: Kind::Scalar { text, value, plain },
        mark,
        size: 1,
        height: 0,
    })
}

fn digits(s: &str, radix: u32) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_digit(radix))
}

fn is_float(text: &str) -> bool {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (unsigned, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", frac)) => digits(frac, 10),
        Some((int, frac)) => digits(int, 10) && (frac.is_empty() || digits(frac, 10)),
        None => digits(mantissa, 10),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e), 10));
    mantissa_ok && exponent_ok
}

fn resolve(text: &str) -> Result<Scalar, String> {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Scalar::Null),
        "true" | "True" | "TRUE" => return Ok(Scalar::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Scalar::Bool(false)),
        ".nan" | ".NaN" | ".NAN" => return Ok(Scalar::Float(f64::NAN)),
        _ => {}
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if matches!(unsigned, ".inf" | ".Inf" | ".INF") {
        return Ok(Scalar::Float(if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }));
    }
    for (prefix, radix) in [("0o", 8), ("0x", 16)] {
        if let Some(body) = text.strip_prefix(prefix)
            && digits(body, radix)
        {
            let n = u32::from_str_radix(body, radix)
                .map_err(|_| format!("`{text}` does not fit in 32 bits"))?
                .cast_signed();
            return Ok(
                u64::try_from(n).map_or_else(|_| Scalar::Signed(i64::from(n)), Scalar::Unsigned)
            );
        }
    }
    if digits(unsigned, 10) {
        if let Ok(n) = text.parse::<u64>() {
            return Ok(Scalar::Unsigned(n));
        }
        if let Ok(n) = text.parse::<i64>() {
            return Ok(Scalar::Signed(n));
        }
    }
    if is_float(text)
        && let Ok(n) = text.parse::<f64>()
    {
        return Ok(Scalar::Float(n));
    }
    Ok(Scalar::Str)
}

fn fold_case(text: &str) -> String {
    text.chars()
        .map(|c| {
            simple_upper::SIMPLE_UPPER
                .binary_search_by_key(&c, |&(lower, _)| lower)
                .ok()
                .and_then(|i| simple_upper::SIMPLE_UPPER.get(i))
                .map_or(c, |&(_, upper)| upper)
        })
        .collect()
}

impl<'a> IntoDeserializer<'_, Error> for &'a Node {
    type Deserializer = &'a Node;

    fn into_deserializer(self) -> Self::Deserializer {
        self
    }
}

impl<'de> Deserializer<'de> for &Node {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.kind {
            Kind::Scalar { text, value, .. } => match *value {
                Scalar::Null => visitor.visit_unit(),
                Scalar::Bool(b) => visitor.visit_bool(b),
                Scalar::Unsigned(n) => visitor.visit_u64(n),
                Scalar::Signed(n) => visitor.visit_i64(n),
                Scalar::Float(n) => visitor.visit_f64(n),
                Scalar::Str => visitor.visit_str(text),
            },
            Kind::Seq(items) => {
                let mut seq = SeqDeserializer::new(items.iter());
                visitor
                    .visit_seq(&mut seq)
                    .and_then(|value| seq.end().map(|()| value))
            }
            Kind::Map(entries) => {
                let mut map = MapDeserializer::new(entries.iter().map(|(k, v)| (k, v)));
                visitor
                    .visit_map(&mut map)
                    .and_then(|value| map.end().map(|()| value))
            }
        }
        .map_err(|e| e.located(self.mark))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.kind {
            Kind::Scalar {
                value: Scalar::Null,
                ..
            } => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
        .map_err(|e| e.located(self.mark))
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.kind {
            Kind::Scalar { text, value, .. } if *value != Scalar::Null => visitor
                .visit_str::<Error>(text)
                .map_err(|e| e.located(self.mark)),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match &self.kind {
            Kind::Scalar {
                text,
                value: Scalar::Str,
                ..
            } => {
                let variant: de::value::StrDeserializer<'_, Error> =
                    text.as_str().into_deserializer();
                variant
                    .deserialize_enum(name, variants, visitor)
                    .map_err(|e| e.located(self.mark))
            }
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor
            .visit_newtype_struct(self)
            .map_err(|e| e.located(self.mark))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char bytes byte_buf
        unit unit_struct seq tuple tuple_struct map struct ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_scalars_resolve_under_the_core_schema() {
        let cases = [
            ("", Scalar::Null),
            ("~", Scalar::Null),
            ("null", Scalar::Null),
            ("Null", Scalar::Null),
            ("NULL", Scalar::Null),
            ("nULL", Scalar::Str),
            ("true", Scalar::Bool(true)),
            ("True", Scalar::Bool(true)),
            ("TRUE", Scalar::Bool(true)),
            ("false", Scalar::Bool(false)),
            ("False", Scalar::Bool(false)),
            ("FALSE", Scalar::Bool(false)),
            ("tRUE", Scalar::Str),
            ("yes", Scalar::Str),
            ("No", Scalar::Str),
            ("on", Scalar::Str),
            ("OFF", Scalar::Str),
            ("0", Scalar::Unsigned(0)),
            ("+12", Scalar::Unsigned(12)),
            ("-12", Scalar::Signed(-12)),
            ("0755", Scalar::Unsigned(755)),
            ("0o17", Scalar::Unsigned(15)),
            ("0o18", Scalar::Str),
            ("0x1F", Scalar::Unsigned(31)),
            ("0x1g", Scalar::Str),
            ("-0x1", Scalar::Str),
            ("0x7FFFFFFF", Scalar::Unsigned(2_147_483_647)),
            ("0o17777777777", Scalar::Unsigned(2_147_483_647)),
            ("0x80000000", Scalar::Signed(-2_147_483_648)),
            ("0xFFFFFFFF", Scalar::Signed(-1)),
            ("0o37777777777", Scalar::Signed(-1)),
            ("0x0000000000001", Scalar::Unsigned(1)),
            ("1_000", Scalar::Str),
            ("1.5", Scalar::Float(1.5)),
            ("-.5", Scalar::Float(-0.5)),
            ("1.", Scalar::Float(1.0)),
            ("1e3", Scalar::Float(1000.0)),
            ("2.5E-1", Scalar::Float(0.25)),
            (
                "18446744073709551616",
                Scalar::Float(18_446_744_073_709_551_616.0),
            ),
            (".inf", Scalar::Float(f64::INFINITY)),
            ("+.Inf", Scalar::Float(f64::INFINITY)),
            ("-.INF", Scalar::Float(f64::NEG_INFINITY)),
            (".", Scalar::Str),
            ("e3", Scalar::Str),
            ("1e", Scalar::Str),
            ("inf", Scalar::Str),
            ("nan", Scalar::Str),
            ("-.nan", Scalar::Str),
            ("v1.2", Scalar::Str),
        ];
        for (text, expected) in cases {
            assert_eq!(resolve(text), Ok(expected), "{text:?}");
        }
        for text in [".nan", ".NaN", ".NAN"] {
            assert!(
                matches!(resolve(text), Ok(Scalar::Float(n)) if n.is_nan()),
                "{text}"
            );
        }
        for text in ["0x100000000", "0o40000000000", "0x10000000000000000"] {
            assert_eq!(
                resolve(text),
                Err(format!("`{text}` does not fit in 32 bits"))
            );
        }
    }

    #[test]
    fn out_of_range_hex_fails_at_its_scalar() -> Result<(), Error> {
        let wrapped: HashMap<String, i64> = from_str("a: 0xFFFFFFFF\n", Dialect::Runner)?;
        assert_eq!(wrapped.get("a"), Some(&-1));
        let Err(err) = from_str::<HashMap<String, i64>>("a: 1\nb: 0x100000000\n", Dialect::Config)
        else {
            panic!("0x100000000 must be rejected");
        };
        assert_eq!(
            err.to_string(),
            "`0x100000000` does not fit in 32 bits at line 2, column 4"
        );
        Ok(())
    }

    #[test]
    fn nesting_stops_at_the_depth_limit() -> Result<(), Error> {
        let block = |depth: usize| {
            (0..depth)
                .map(|level| format!("{}-", "  ".repeat(level)))
                .collect::<Vec<_>>()
                .join("\n")
                + " x\n"
        };
        let flow = |depth: usize| format!("{}x{}", "[".repeat(depth), "]".repeat(depth));
        for (dialect, max) in [(Dialect::Config, 128), (Dialect::Runner, 100)] {
            assert_eq!(dialect.max_depth(), max);
            from_str::<figment::value::Value>(&block(max), dialect)?;
            from_str::<figment::value::Value>(&flow(max), dialect)?;
            for (text, line, column) in [
                (block(max + 1), max + 1, 2 * max + 1),
                (flow(200), 1, max + 1),
            ] {
                let Err(err) = from_str::<figment::value::Value>(&text, dialect) else {
                    panic!("nesting past {max} must fail");
                };
                assert_eq!(
                    err.to_string(),
                    format!(
                        "sequences and mappings nest deeper than {max} levels at line {line}, column {column}"
                    )
                );
            }
        }
        let aliased = format!("a: &a {}\nb: [*a]\n", flow(127));
        let Err(err) = from_str::<figment::value::Value>(&aliased, Dialect::Config) else {
            panic!("an alias past 128 levels must fail");
        };
        assert!(
            err.to_string()
                .ends_with("nest deeper than 128 levels at line 2, column 5"),
            "{err}"
        );
        Ok(())
    }

    #[test]
    fn alias_inside_its_own_anchor_fails() {
        let Err(err) = from_str::<figment::value::Value>("a: &a [*a]\n", Dialect::Config) else {
            panic!("a self alias must fail");
        };
        assert_eq!(
            err.to_string(),
            "alias names an anchor whose node is not complete at line 1, column 8"
        );
    }

    #[test]
    fn runner_keys_compare_as_dotnet_ordinal_ignore_case() -> Result<(), Error> {
        let text = "Token: a\ntoken: b\n";
        let config: HashMap<String, String> = from_str(text, Dialect::Config)?;
        assert_eq!(config.len(), 2);
        for (upper, lower) in [
            ("Token", "token"),
            ("É", "é"),
            ("\u{1F88}", "\u{1F80}"),
            ("\u{1FBC}", "\u{1FB3}"),
            ("\u{1FCC}", "\u{1FC3}"),
            ("\u{1FFC}", "\u{1FF3}"),
            ("\u{01C4}", "\u{01C5}"),
            ("\u{01C5}", "\u{01C6}"),
            ("\u{10400}", "\u{10428}"),
        ] {
            let text = format!("{upper}: a\n{lower}: b\n");
            let Err(err) = from_str::<HashMap<String, String>>(&text, Dialect::Runner) else {
                panic!("{text:?} must be rejected");
            };
            assert!(
                err.to_string()
                    .ends_with(&format!("duplicate key `{lower}` at line 2, column 1")),
                "{err}"
            );
        }
        for (a, b) in [
            ("ss", "ß"),
            ("i", "\u{0131}"),
            ("I", "\u{0131}"),
            ("s", "\u{017F}"),
            ("S", "\u{017F}"),
            ("\u{10D50}", "\u{10D70}"),
            ("\u{16EA0}", "\u{16EBB}"),
            ("${{ a }}", "${{ a }}"),
            ("x-${{ A }}", "x-${{ a }}"),
        ] {
            let text = format!("{a}: a\n{b}: b\n");
            let loaded = from_str::<figment::value::Value>(&text, Dialect::Runner);
            assert!(loaded.is_ok(), "{text:?}: {loaded:?}");
        }
        Ok(())
    }

    #[test]
    fn runner_ignores_tags_on_collections() -> Result<(), Error> {
        for text in ["x: !custom {a: 1}\n", "x: !!str [1]\n", "x: !!seq {a: 1}\n"] {
            from_str::<figment::value::Value>(text, Dialect::Runner)?;
            let Err(err) = from_str::<figment::value::Value>(text, Dialect::Config) else {
                panic!("{text:?} must be rejected in config");
            };
            assert!(err.to_string().contains("tag `"), "{err}");
        }
        Ok(())
    }

    #[test]
    fn anchors_expand_or_are_rejected() -> Result<(), Error> {
        let text = "a: &x [1, 2]\nb: *x\n";
        let expanded: HashMap<String, Vec<u8>> = from_str(text, Dialect::Config)?;
        assert_eq!(expanded.get("b"), Some(&vec![1, 2]));
        let Err(err) = from_str::<HashMap<String, Vec<u8>>>(text, Dialect::Runner) else {
            panic!("anchors must be rejected");
        };
        assert_eq!(
            err.to_string(),
            "anchors are not supported at line 1, column 7"
        );
        Ok(())
    }

    #[derive(Debug, serde::Deserialize)]
    struct Doc {
        a: Vec<u8>,
        b: Vec<Vec<u8>>,
    }

    fn aliases(count: usize) -> String {
        format!(
            "a: &a [{}]\nb: [{}]\n",
            ["1"; 9].join(", "),
            vec!["*a"; count].join(", ")
        )
    }

    #[test]
    fn alias_expansion_stops_at_the_cap() -> Result<(), Error> {
        let doc: Doc = from_str(&aliases(1000), Dialect::Config)?;
        assert_eq!((doc.a.len(), doc.b.len()), (9, 1000));
        let Err(err) = from_str::<Doc>(&aliases(1001), Dialect::Config) else {
            panic!("expansion past the cap must fail");
        };
        assert!(
            err.to_string()
                .starts_with("aliases expand to more than 10000 nodes at line 2"),
            "{err}"
        );
        Ok(())
    }
    #[derive(Debug, PartialEq, serde::Deserialize)]
    enum Mode {
        Fast,
        Slow,
    }

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct Port(u16);

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct Typed {
        mode: Mode,
        port: Port,
        name: String,
    }

    #[test]
    fn enums_newtypes_and_strings_deserialize() -> Result<(), Error> {
        let typed: Typed = from_str("mode: Slow\nport: 8080\nname: 0x1F\n", Dialect::Runner)?;
        assert_eq!(
            typed,
            Typed {
                mode: Mode::Slow,
                port: Port(8080),
                name: "0x1F".to_owned(),
            }
        );
        for (text, needle) in [
            (
                "mode: Medium\nport: 1\nname: n\n",
                "unknown variant `Medium`, expected `Fast` or `Slow` at line 1, column 7",
            ),
            ("mode: 1\nport: 1\nname: n\n", "at line 1, column 7"),
            (
                "mode: Fast\nport: 1\nname: ~\n",
                "invalid type: unit value, expected a string at line 3, column 7",
            ),
            (
                "mode: Fast\nport: x\nname: n\n",
                "invalid type: string \"x\", expected u16 at line 2, column 7",
            ),
            (
                "mode: Fast\nname: n\n",
                "missing field `port` at line 1, column 1",
            ),
        ] {
            let Err(err) = from_str::<Typed>(text, Dialect::Runner) else {
                panic!("{text:?} must be rejected");
            };
            assert!(err.to_string().contains(needle), "{text:?}: {err}");
        }
        Ok(())
    }

    #[test]
    fn core_tags_type_their_node() -> Result<(), Error> {
        let parsed: HashMap<String, figment::value::Value> = from_str(
            "f: !!float 1\ni: !!int 12\nb: !!bool True\nn: !!null ~\ns: !!str true\nm: !!map {}\nq: !!seq []\nt: !!str 'quoted'\n",
            Dialect::Runner,
        )?;
        let num = |key: &str| parsed.get(key).and_then(figment::value::Value::to_num);
        assert_eq!(num("f"), Some(figment::value::Num::F64(1.0)));
        assert_eq!(num("i"), Some(figment::value::Num::U64(12)));
        assert_eq!(
            parsed.get("b").and_then(figment::value::Value::to_bool),
            Some(true)
        );
        assert!(matches!(
            parsed.get("n"),
            Some(figment::value::Value::Empty(..))
        ));
        assert_eq!(
            parsed.get("s").and_then(figment::value::Value::as_str),
            Some("true")
        );
        for (text, needle) in [
            (
                "x: !!float abc\n",
                "`abc` does not fit tag `!!float` at line 1, column 12",
            ),
            ("x: !!int 1.5\n", "`1.5` does not fit tag `!!int`"),
            ("x: !!bool yes\n", "`yes` does not fit tag `!!bool`"),
            ("x: !!null 0\n", "`0` does not fit tag `!!null`"),
            ("x: !!map abc\n", "`abc` does not fit tag `!!map`"),
            (
                "x: !!int \"12\"\n",
                "tag `!!int` needs a plain scalar at line 1, column 10",
            ),
            ("x: !!bool 'true'\n", "tag `!!bool` needs a plain scalar"),
            ("x: !!null \"\"\n", "tag `!!null` needs a plain scalar"),
            ("x: !!int |\n  1\n", "tag `!!int` needs a plain scalar"),
            (
                "x: !!str\n  y: 1\n",
                "tag `!!str` does not fit this node at line 2, column 3",
            ),
            ("x: !!seq {}\n", "tag `!!seq` does not fit this node"),
            (
                "x: !!timestamp 2001-12-14\n",
                "tag `!!timestamp` is not a core schema tag",
            ),
        ] {
            let Err(err) =
                from_str::<HashMap<String, figment::value::Value>>(text, Dialect::Config)
            else {
                panic!("{text:?} must be rejected");
            };
            assert!(err.to_string().contains(needle), "{text:?}: {err}");
        }
        Ok(())
    }
}

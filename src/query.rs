//! Search expression parsing and parameterized SQLite predicates.
//!
//! Whitespace means AND; `|` / OR and `!` / NOT compose with parentheses.
//! Quotes preserve backslashes (including a Windows directory's trailing `\`);
//! a doubled quote inside a quoted string represents a literal quote.
//! Plain text is a substring. Wildcards match the whole name/path. Global regex
//! mode treats the complete input as one regex; `regex:"..."` composes with filters.

use crate::model::SearchOptions;
use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Days, Local, NaiveDate, TimeZone};
use regex::{Regex, RegexBuilder};
use rusqlite::{Connection, functions::FunctionFlags, types::Value};

const MAX_QUERY_BYTES: usize = 16_384;
const MAX_TOKENS: usize = 512;
const MAX_DEPTH: usize = 32;
const REGEX_SIZE_LIMIT: usize = 4 * 1024 * 1024;
const MAX_FTS_TERMS: usize = 8;

#[derive(Debug)]
pub struct CompiledQuery {
    pub where_sql: String,
    pub params: Vec<Value>,
}

pub fn compile(text: &str, options: &SearchOptions) -> Result<CompiledQuery> {
    ensure!(
        text.len() <= MAX_QUERY_BYTES,
        "Search is too long (maximum {MAX_QUERY_BYTES} bytes)"
    );
    let mut compiler = Compiler { params: Vec::new() };
    let where_sql = if text.is_empty() || (!options.regex && text.trim().is_empty()) {
        "1".to_owned()
    } else if options.regex {
        compiler.regex(text, *options, None)?
    } else {
        let mut parser = Parser {
            tokens: tokenize(text)?,
            position: 0,
        };
        let expression = parser.parse_or(0)?;
        ensure!(
            parser.position == parser.tokens.len(),
            "Unexpected closing parenthesis or operator"
        );
        compiler.expression(&expression, *options, None, 0)?
    };
    Ok(CompiledQuery {
        where_sql,
        params: compiler.params,
    })
}

pub fn register_functions(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "query_regex",
        2,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let regex = context.get_or_create_aux(
                0,
                |value| -> std::result::Result<Regex, Box<dyn std::error::Error + Send + Sync>> {
                    Ok(build_regex(value.as_str()?)?)
                },
            )?;
            let text = context
                .get_raw(1)
                .as_str()
                .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
            Ok(regex.is_match(text))
        },
    )?;
    Ok(())
}

fn build_regex(pattern: &str) -> std::result::Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .nest_limit(128)
        .build()
}

#[derive(Clone, Debug)]
struct Atom {
    text: String,
    /// Byte offsets of colons outside quotes. Quoted `"ext:txt"` stays literal.
    colons: Vec<usize>,
    quoted: bool,
}

impl Atom {
    fn function(&self) -> Option<(&str, Self)> {
        let colon = *self.colons.first()?;
        if colon == 1 && self.text.as_bytes()[0].is_ascii_alphabetic() {
            return None; // C:, C:\path, and drive-relative C:path are paths.
        }
        Some((
            &self.text[..colon],
            Self {
                text: self.text[colon + 1..].to_owned(),
                colons: self.colons[1..]
                    .iter()
                    .map(|offset| offset - colon - 1)
                    .collect(),
                quoted: self.quoted,
            },
        ))
    }

    fn append(&mut self, other: Self) {
        let offset = self.text.len();
        self.colons
            .extend(other.colons.into_iter().map(|colon| colon + offset));
        self.text.push_str(&other.text);
        self.quoted |= other.quoted;
    }
}

#[derive(Clone, Debug)]
enum Token {
    Atom(Atom),
    And,
    Or,
    Not,
    Left,
    Right,
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let mut characters = text.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(&character) = characters.peek() {
        if character.is_whitespace() {
            characters.next();
            continue;
        }
        let punctuation = match character {
            '(' => Some(Token::Left),
            ')' => Some(Token::Right),
            '|' => Some(Token::Or),
            '!' => Some(Token::Not),
            _ => None,
        };
        if let Some(token) = punctuation {
            characters.next();
            tokens.push(token);
        } else {
            let mut atom = Atom {
                text: String::new(),
                colons: Vec::new(),
                quoted: false,
            };
            let mut inside_quote = false;
            while let Some(&character) = characters.peek() {
                if !inside_quote
                    && (character.is_whitespace() || matches!(character, '(' | ')' | '|'))
                {
                    break;
                }
                characters.next();
                if character == '"' {
                    atom.quoted = true;
                    if inside_quote && characters.peek() == Some(&'"') {
                        characters.next();
                        atom.text.push('"');
                    } else {
                        inside_quote = !inside_quote;
                    }
                } else {
                    if !inside_quote && character == ':' {
                        atom.colons.push(atom.text.len());
                    }
                    atom.text.push(character);
                }
            }
            ensure!(!inside_quote, "Unclosed quoted string");
            let token = if !atom.quoted && atom.text.eq_ignore_ascii_case("AND") {
                Token::And
            } else if !atom.quoted && atom.text.eq_ignore_ascii_case("OR") {
                Token::Or
            } else if !atom.quoted && atom.text.eq_ignore_ascii_case("NOT") {
                Token::Not
            } else {
                Token::Atom(atom)
            };
            tokens.push(token);
        }
        ensure!(
            tokens.len() <= MAX_TOKENS,
            "Search contains too many terms or operators (maximum {MAX_TOKENS} tokens)"
        );
    }
    Ok(tokens)
}

#[derive(Debug)]
enum Expression {
    Atom(Atom),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
    Scope(Atom, Box<Self>),
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn parse_or(&mut self, depth: usize) -> Result<Expression> {
        let mut parts = vec![self.parse_and(depth)?];
        while matches!(self.tokens.get(self.position), Some(Token::Or)) {
            self.position += 1;
            parts.push(self.parse_and(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expression::Or(parts)
        })
    }

    fn parse_and(&mut self, depth: usize) -> Result<Expression> {
        let mut parts = vec![self.parse_unary(depth)?];
        loop {
            match self.tokens.get(self.position) {
                Some(Token::And) => {
                    self.position += 1;
                }
                Some(Token::Atom(_) | Token::Not | Token::Left) => {}
                _ => break,
            }
            parts.push(self.parse_unary(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expression::And(parts)
        })
    }

    fn parse_unary(&mut self, depth: usize) -> Result<Expression> {
        ensure!(
            depth < MAX_DEPTH,
            "Search nesting is too deep (maximum {MAX_DEPTH})"
        );
        let token = self
            .tokens
            .get(self.position)
            .cloned()
            .context("Expected a search term after an operator")?;
        self.position += 1;
        match token {
            Token::Not => Ok(Expression::Not(Box::new(self.parse_unary(depth + 1)?))),
            Token::Left => {
                let expression = self.parse_or(depth + 1)?;
                ensure!(
                    matches!(self.tokens.get(self.position), Some(Token::Right)),
                    "Missing closing parenthesis"
                );
                self.position += 1;
                Ok(expression)
            }
            Token::Atom(mut atom) => {
                if atom.function().is_some()
                    && atom.colons.last().copied() == atom.text.len().checked_sub(1)
                {
                    if matches!(self.tokens.get(self.position), Some(Token::Left)) {
                        let expression = self.parse_unary(depth + 1)?;
                        return Ok(Expression::Scope(atom, Box::new(expression)));
                    }
                    if let Some((function, value)) = atom.function() {
                        let requires_value = !matches!(
                            function.to_ascii_lowercase().as_str(),
                            "file"
                                | "folder"
                                | "audio"
                                | "video"
                                | "pic"
                                | "doc"
                                | "exe"
                                | "compressed"
                        );
                        if requires_value
                            && value.text.is_empty()
                            && let Some(Token::Atom(value)) =
                                self.tokens.get(self.position).cloned()
                        {
                            self.position += 1;
                            atom.append(value);
                        }
                    }
                }
                Ok(Expression::Atom(atom))
            }
            _ => bail!("Expected a search term; found an operator or closing parenthesis"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Path,
    Parent,
}

impl Field {
    fn column(self, folded: bool) -> &'static str {
        match (self, folded) {
            (Self::Name, false) => "f.name",
            (Self::Name, true) => "f.name_fold",
            (Self::Path, false) => "f.path",
            (Self::Path, true) => "f.path_fold",
            (Self::Parent, false) => "f.parent",
            (Self::Parent, true) => "f.parent_fold",
        }
    }
}

struct Compiler {
    params: Vec<Value>,
}

impl Compiler {
    fn expression(
        &mut self,
        expression: &Expression,
        options: SearchOptions,
        field: Option<Field>,
        depth: usize,
    ) -> Result<String> {
        ensure!(depth < MAX_DEPTH * 3, "Search nesting is too deep");
        match expression {
            Expression::Atom(atom) => self.atom(atom, options, field, depth + 1),
            Expression::Not(expression) => Ok(format!(
                "(NOT {})",
                self.expression(expression, options, field, depth + 1)?
            )),
            Expression::And(parts) | Expression::Or(parts) => {
                let separator = if matches!(expression, Expression::And(_)) {
                    " AND "
                } else {
                    " OR "
                };
                let predicates = parts
                    .iter()
                    .map(|part| self.expression(part, options, field, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("({})", predicates.join(separator)))
            }
            Expression::Scope(prefix, expression) => {
                self.scope(prefix, expression, options, field, depth + 1)
            }
        }
    }

    fn scope(
        &mut self,
        prefix: &Atom,
        expression: &Expression,
        mut options: SearchOptions,
        mut field: Option<Field>,
        depth: usize,
    ) -> Result<String> {
        ensure!(depth < MAX_DEPTH * 3, "Search function nesting is too deep");
        let (function, rest) = prefix
            .function()
            .context("Invalid function before a group")?;
        let filter = match function.to_ascii_lowercase().as_str() {
            "case" => {
                options.case_sensitive = true;
                None
            }
            "regex" => {
                options.regex = true;
                None
            }
            "path" => {
                field = Some(Field::Path);
                None
            }
            "file" => Some("f.is_dir = 0".to_owned()),
            "folder" => Some("f.is_dir = 1".to_owned()),
            name if preset(name).is_some() => Some(self.extensions(preset(name).unwrap())?),
            "content" => bail!(
                "content: is not supported; this index contains names and metadata, not file contents"
            ),
            _ => bail!("Function {function}: cannot be applied to a group"),
        };
        let predicate = if rest.text.is_empty() {
            self.expression(expression, options, field, depth + 1)?
        } else {
            self.scope(&rest, expression, options, field, depth + 1)?
        };
        Ok(match filter {
            Some(filter) => format!("({filter} AND {predicate})"),
            None => predicate,
        })
    }

    fn atom(
        &mut self,
        atom: &Atom,
        mut options: SearchOptions,
        field: Option<Field>,
        depth: usize,
    ) -> Result<String> {
        ensure!(depth < MAX_DEPTH * 3, "Search function nesting is too deep");
        let Some((function, value)) = atom.function() else {
            return self.literal(&atom.text, options, field);
        };
        let function = function.to_ascii_lowercase();
        match function.as_str() {
            "case" => {
                require_value(&function, &value.text)?;
                options.case_sensitive = true;
                self.atom(&value, options, field, depth + 1)
            }
            "regex" => {
                ensure!(
                    !value.text.is_empty() || value.quoted,
                    "regex: requires a pattern"
                );
                self.regex(&value.text, options, field)
            }
            "path" => {
                require_value(&function, &value.text)?;
                self.atom(&value, options, Some(Field::Path), depth + 1)
            }
            "parent" => {
                require_value(&function, &value.text)?;
                let parent = trim_parent_separator(&value.text);
                let column = Field::Parent.column(!options.case_sensitive);
                self.params.push(Value::Text(if options.case_sensitive {
                    parent.to_owned()
                } else {
                    parent.to_lowercase()
                }));
                Ok(format!("{column} = ?"))
            }
            "file" | "folder" => {
                let filter = if function == "file" {
                    "f.is_dir = 0"
                } else {
                    "f.is_dir = 1"
                };
                if value.text.is_empty() {
                    Ok(filter.to_owned())
                } else {
                    Ok(format!(
                        "({filter} AND {})",
                        self.atom(&value, options, field, depth + 1)?
                    ))
                }
            }
            "ext" => self.extensions(&value.text),
            "size" => self.size(&value.text),
            "dm" | "date-modified" => self.date("f.modified", &value.text),
            "dc" | "date-created" => self.date("f.created", &value.text),
            "content" => bail!(
                "content: is not supported; this index contains names and metadata, not file contents"
            ),
            name if preset(name).is_some() => {
                let filter = self.extensions(preset(name).unwrap())?;
                if value.text.is_empty() {
                    Ok(filter)
                } else {
                    Ok(format!(
                        "({filter} AND {})",
                        self.atom(&value, options, field, depth + 1)?
                    ))
                }
            }
            _ => {
                bail!("Unknown search function {function}:; quote text containing a literal colon")
            }
        }
    }

    fn literal(
        &mut self,
        text: &str,
        options: SearchOptions,
        field: Option<Field>,
    ) -> Result<String> {
        if options.regex {
            return self.regex(text, options, field);
        }
        let field = field.unwrap_or_else(|| {
            if options.match_path || text.contains(['/', '\\']) || is_drive_path(text) {
                Field::Path
            } else {
                Field::Name
            }
        });
        if text.contains(['*', '?']) {
            let mut pattern = String::new();
            for character in text.chars() {
                match character {
                    '*' => pattern.push_str(".*"),
                    '?' => pattern.push('.'),
                    _ => pattern.push_str(&regex::escape(&character.to_string())),
                }
            }
            let pattern = format!("(?s:\\A(?:{pattern})\\z)");
            // Wildcards run on original Unicode characters, so ? consumes one
            // character even when lowercase expands it (e.g. capital dotted I).
            // Regex Unicode folding includes equivalences absent from the stored
            // lowercase copy (ſ/s), so an insensitive FTS candidate is unsafe.
            return self.regex(&pattern, options, Some(field));
        }
        let normalized = if options.case_sensitive {
            text.to_owned()
        } else {
            text.to_lowercase()
        };
        let candidate = if field != Field::Parent {
            trigram_match(&normalized, options.case_sensitive)
        } else {
            None
        };
        let column = field.column(!options.case_sensitive);
        let prefix = if let Some(candidate) = candidate {
            self.params.push(Value::Text(candidate));
            "f.id IN (SELECT rowid FROM file_fts WHERE file_fts MATCH ?) AND "
        } else {
            ""
        };
        if options.whole_word {
            let pattern = format!("(?:\\A|\\W)(?:{})(?:\\z|\\W)", regex::escape(&normalized));
            build_regex(&pattern).context("Invalid whole-word search")?;
            self.params.push(Value::Text(pattern));
            Ok(format!("({prefix}query_regex(?, {column}))"))
        } else {
            self.params.push(Value::Text(normalized));
            Ok(format!("({prefix}instr({column}, ?) > 0)"))
        }
    }

    fn regex(
        &mut self,
        text: &str,
        options: SearchOptions,
        field: Option<Field>,
    ) -> Result<String> {
        let mut pattern = if options.case_sensitive {
            text.to_owned()
        } else {
            format!("(?i:{text})")
        };
        if options.whole_word {
            pattern = format!("(?:\\A|\\W)(?:{pattern})(?:\\z|\\W)");
        }
        build_regex(&pattern).context("Invalid regular expression")?;
        self.params.push(Value::Text(pattern));
        let column = field
            .unwrap_or(if options.match_path {
                Field::Path
            } else {
                Field::Name
            })
            .column(false);
        Ok(format!("query_regex(?, {column})"))
    }

    fn extensions(&mut self, text: &str) -> Result<String> {
        require_value("ext", text)?;
        let extensions = text
            .split([';', ','])
            .map(|extension| extension.trim().trim_start_matches('.').to_lowercase())
            .collect::<Vec<_>>();
        ensure!(
            extensions.iter().all(|extension| !extension.is_empty()),
            "ext: contains an empty extension"
        );
        let placeholders = vec!["?"; extensions.len()].join(",");
        self.params.extend(extensions.into_iter().map(Value::Text));
        Ok(format!(
            "(f.is_dir = 0 AND f.extension IN ({placeholders}))"
        ))
    }

    fn size(&mut self, text: &str) -> Result<String> {
        require_value("size", text)?;
        let (comparison, value, explicit) = comparison(text);
        if let Some((lower, upper)) = value.split_once("..") {
            ensure!(
                !explicit && !upper.contains(".."),
                "Use size:minimum..maximum without a comparison operator"
            );
            let lower = if lower.is_empty() {
                None
            } else {
                Some(parse_size(lower)?)
            };
            let upper = if upper.is_empty() {
                None
            } else {
                Some(parse_size(upper)?)
            };
            ensure!(
                lower.is_some() || upper.is_some(),
                "Size range requires a bound"
            );
            if let (Some(lower), Some(upper)) = (lower, upper) {
                ensure!(lower <= upper, "Size range is reversed");
            }
            let mut predicates = vec!["f.size IS NOT NULL".to_owned()];
            if let Some(lower) = lower {
                self.params.push(Value::Integer(lower));
                predicates.push("f.size >= ?".to_owned());
            }
            if let Some(upper) = upper {
                self.params.push(Value::Integer(upper));
                predicates.push("f.size <= ?".to_owned());
            }
            Ok(format!("({})", predicates.join(" AND ")))
        } else {
            self.params.push(Value::Integer(parse_size(value)?));
            Ok(format!(
                "(f.size IS NOT NULL AND f.size {} ?)",
                comparison.sql()
            ))
        }
    }

    fn date(&mut self, column: &'static str, text: &str) -> Result<String> {
        require_value("date", text)?;
        let (comparison, value, explicit) = comparison(text);
        let mut predicates = vec![format!("{column} IS NOT NULL")];
        if let Some((lower, upper)) = value.split_once("..") {
            ensure!(
                !explicit && !upper.contains(".."),
                "Use date:from..to without a comparison operator"
            );
            let lower = if lower.is_empty() {
                None
            } else {
                Some(date_span(lower)?.0)
            };
            let upper = if upper.is_empty() {
                None
            } else {
                Some(date_span(upper)?.1)
            };
            ensure!(
                lower.is_some() || upper.is_some(),
                "Date range requires a bound"
            );
            if let (Some(lower), Some(upper)) = (lower, upper) {
                ensure!(lower < upper, "Date range is reversed or empty");
            }
            if let Some(lower) = lower {
                self.params.push(Value::Integer(lower));
                predicates.push(format!("{column} >= ?"));
            }
            if let Some(upper) = upper {
                self.params.push(Value::Integer(upper));
                predicates.push(format!("{column} < ?"));
            }
        } else {
            let (start, end) = date_span(value)?;
            match comparison {
                Comparison::Equal | Comparison::NotEqual => {
                    self.params.push(Value::Integer(start));
                    self.params.push(Value::Integer(end));
                    predicates.push(if comparison == Comparison::Equal {
                        format!("({column} >= ? AND {column} < ?)")
                    } else {
                        format!("({column} < ? OR {column} >= ?)")
                    });
                }
                Comparison::Less | Comparison::GreaterEqual => {
                    self.params.push(Value::Integer(start));
                    predicates.push(format!(
                        "{column} {} ?",
                        if comparison == Comparison::Less {
                            "<"
                        } else {
                            ">="
                        }
                    ));
                }
                Comparison::LessEqual | Comparison::Greater => {
                    self.params.push(Value::Integer(end));
                    predicates.push(format!(
                        "{column} {} ?",
                        if comparison == Comparison::LessEqual {
                            "<"
                        } else {
                            ">="
                        }
                    ));
                }
            }
        }
        Ok(format!("({})", predicates.join(" AND ")))
    }
}

fn require_value(function: &str, text: &str) -> Result<()> {
    ensure!(!text.trim().is_empty(), "{function}: requires a value");
    Ok(())
}

fn is_drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn trim_parent_separator(text: &str) -> &str {
    let trimmed = text.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        &text[..text.chars().next().map(char::len_utf8).unwrap_or(0)]
    } else if trimmed.len() == 2 && is_drive_path(trimmed) && text.len() > 2 {
        &text[..3]
    } else {
        trimmed
    }
}

fn trigram_match(text: &str, case_sensitive: bool) -> Option<String> {
    if text.contains('\0') {
        return None;
    }
    let characters = text.chars().collect::<Vec<_>>();
    let windows = characters
        .windows(3)
        .filter_map(|window| {
            // Context-sensitive lowercase (notably Greek final sigma) makes a
            // folded candidate unsafe for arbitrary case-sensitive Unicode text.
            // An ASCII window is context-independent. Insensitive predicates use
            // the exact stored fold, so all Unicode windows are safe for those.
            if case_sensitive && !window.iter().all(char::is_ascii) {
                return None;
            }
            Some([window[0], window[1], window[2]].map(|character| {
                if case_sensitive {
                    character.to_ascii_lowercase()
                } else {
                    character
                }
            }))
        })
        .collect::<Vec<_>>();
    let last = windows.len().checked_sub(1)?;
    // Common prefixes such as "invoice_" or "project_" can admit the entire
    // index. Spread a bounded set across the literal, including the final two
    // windows so a distinguishing suffix such as 0042 contributes 004 AND 042.
    let positions = if windows.len() <= MAX_FTS_TERMS {
        (0..windows.len()).collect::<Vec<_>>()
    } else {
        (0..MAX_FTS_TERMS - 1)
            .map(|index| index * (last - 1) / (MAX_FTS_TERMS - 2))
            .chain(std::iter::once(last))
            .collect()
    };
    let mut selected = Vec::with_capacity(MAX_FTS_TERMS);
    for position in positions {
        let term = windows[position];
        if !selected.contains(&term) {
            selected.push(term);
        }
    }
    // detail=none cannot match longer phrases. Every quoted term remains
    // exactly three Unicode characters; the final SQL predicate stays exact.
    Some(
        selected
            .iter()
            .map(|term| {
                let term = term.iter().collect::<String>();
                format!("\"{}\"", term.replace('"', "\"\""))
            })
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

fn preset(name: &str) -> Option<&'static str> {
    Some(match name {
        "audio" => "aac;aif;aiff;alac;ape;flac;m4a;mid;midi;mp3;ogg;opus;wav;wma",
        "video" => "3gp;avi;flv;m4v;mkv;mov;mp4;mpeg;mpg;mts;ts;webm;wmv",
        "pic" => "avif;bmp;gif;heic;heif;ico;jpeg;jpg;png;raw;svg;tif;tiff;webp",
        "doc" => "csv;doc;docx;epub;htm;html;md;odf;odp;ods;odt;pdf;ppt;pptx;rtf;tex;txt;xls;xlsx",
        "exe" => "appimage;bat;cmd;com;exe;msi;ps1;sh",
        "compressed" => "7z;bz2;cab;gz;iso;rar;tar;tgz;xz;zip;zst",
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

impl Comparison {
    fn sql(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessEqual => "<=",
            Self::Greater => ">",
            Self::GreaterEqual => ">=",
        }
    }
}

fn comparison(text: &str) -> (Comparison, &str, bool) {
    let text = text.trim();
    for (prefix, comparison) in [
        (">=", Comparison::GreaterEqual),
        ("<=", Comparison::LessEqual),
        ("!=", Comparison::NotEqual),
        (">", Comparison::Greater),
        ("<", Comparison::Less),
        ("=", Comparison::Equal),
    ] {
        if let Some(value) = text.strip_prefix(prefix) {
            return (comparison, value.trim(), true);
        }
    }
    (Comparison::Equal, text, false)
}

fn parse_size(text: &str) -> Result<i64> {
    let text = text.trim();
    let end = text
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(text.len());
    let number = &text[..end];
    let unit = text[end..].trim().to_ascii_lowercase();
    let factor: i128 = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "t" | "tb" => 1_000_000_000_000,
        "p" | "pb" => 1_000_000_000_000_000,
        "e" | "eb" => 1_000_000_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        "pib" => 1 << 50,
        "eib" => 1 << 60,
        _ => bail!("Invalid size unit {unit:?}; use B, KB/MB/GB or KiB/MiB/GiB"),
    };
    let (integer, fraction) = number.split_once('.').unwrap_or((number, ""));
    ensure!(
        !number.is_empty()
            && !(integer.is_empty() && fraction.is_empty())
            && integer.chars().all(|c| c.is_ascii_digit())
            && fraction.chars().all(|c| c.is_ascii_digit()),
        "Invalid size {text:?}"
    );
    ensure!(!number.ends_with('.'), "Invalid size {text:?}");
    let scale = 10_i128
        .checked_pow(
            fraction
                .len()
                .try_into()
                .context("Size has too many digits")?,
        )
        .context("Size has too many decimal places")?;
    let digits: i128 = format!("{integer}{fraction}")
        .parse()
        .context("Invalid or excessively large size")?;
    let scaled = digits
        .checked_mul(factor)
        .context("Size exceeds supported range")?;
    ensure!(
        scaled % scale == 0,
        "Size must resolve to a whole number of bytes"
    );
    i64::try_from(scaled / scale)
        .context("Size exceeds supported range (maximum 9223372036854775807 bytes)")
}

/// Half-open local-calendar interval, converted to the UTC Unix seconds stored
/// by the index. Adding calendar days, rather than 86400 seconds, handles DST.
fn date_span(text: &str) -> Result<(i64, i64)> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let (start, end) = match lower.as_str() {
        "today" | "yesterday" => {
            let today = Local::now().date_naive();
            let start = if lower == "yesterday" {
                today
                    .checked_sub_days(Days::new(1))
                    .context("Date outside supported range")?
            } else {
                today
            };
            (
                start,
                start.succ_opt().context("Date outside supported range")?,
            )
        }
        _ if text.len() == 10 => {
            let start = NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .with_context(|| format!("Invalid date {text:?}; use YYYY-MM-DD"))?;
            (
                start,
                start.succ_opt().context("Date outside supported range")?,
            )
        }
        _ if text.len() == 7 => {
            let start = NaiveDate::parse_from_str(&format!("{text}-01"), "%Y-%m-%d")
                .with_context(|| format!("Invalid month {text:?}; use YYYY-MM"))?;
            let (year, month) = if start.month() == 12 {
                (start.year() + 1, 1)
            } else {
                (start.year(), start.month() + 1)
            };
            (
                start,
                NaiveDate::from_ymd_opt(year, month, 1).context("Date outside supported range")?,
            )
        }
        _ if text.len() == 4 && text.chars().all(|character| character.is_ascii_digit()) => {
            let year: i32 = text.parse().context("Invalid year")?;
            (
                NaiveDate::from_ymd_opt(year, 1, 1).context("Invalid year")?,
                NaiveDate::from_ymd_opt(year + 1, 1, 1).context("Date outside supported range")?,
            )
        }
        _ => bail!("Invalid date {text:?}; use YYYY-MM-DD, YYYY-MM, YYYY, today or yesterday"),
    };
    let boundary = |date: NaiveDate| -> Result<i64> {
        Local
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).context("Invalid calendar date")?)
            .earliest()
            .map(|time| time.timestamp())
            .with_context(|| format!("Midnight on {date} does not exist in the local timezone"))
    };
    Ok((boundary(start)?, boundary(end)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use rusqlite::{params, params_from_iter};

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (
                id INTEGER PRIMARY KEY, path TEXT NOT NULL, name TEXT NOT NULL,
                parent TEXT NOT NULL, extension TEXT NOT NULL, name_fold TEXT NOT NULL,
                path_fold TEXT NOT NULL, parent_fold TEXT NOT NULL, size INTEGER,
                modified INTEGER, created INTEGER, is_dir INTEGER NOT NULL,
                is_symlink INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL DEFAULT ''
            );
            CREATE VIRTUAL TABLE file_fts USING fts5(
                name_fold, path_fold, content='files', content_rowid='id',
                tokenize='trigram', detail='none'
            );",
        )
        .unwrap();
        let rows = [
            (
                1,
                "/data/Report.TXT",
                "Report.TXT",
                "/data",
                "txt",
                Some(1500),
                false,
            ),
            (
                2,
                "/data/report final.txt",
                "report final.txt",
                "/data",
                "txt",
                Some(2048),
                false,
            ),
            (
                3,
                "/data/reports.csv",
                "reports.csv",
                "/data",
                "csv",
                Some(1000),
                false,
            ),
            (
                4,
                "/data/photo.JPG",
                "photo.JPG",
                "/data",
                "jpg",
                Some(1_048_576),
                false,
            ),
            (
                5,
                r"C:\Program Files\App\tool.EXE",
                "tool.EXE",
                r"C:\Program Files\App",
                "exe",
                Some(2000),
                false,
            ),
            (
                6,
                "/data/music/Blue Song.FLAC",
                "Blue Song.FLAC",
                "/data/music",
                "flac",
                Some(0),
                false,
            ),
            (
                7,
                "/data/archive.tar.gz",
                "archive.tar.gz",
                "/data",
                "gz",
                Some(4000),
                false,
            ),
            (8, "/data/docs", "docs", "/data", "", None, true),
            (
                9,
                "/data/İSTANBUL.txt",
                "İSTANBUL.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                10,
                "/data/ΑΣΟΣΑ.txt",
                "ΑΣΟΣΑ.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                11,
                "/data/silly%_name.txt",
                "silly%_name.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                12,
                "/data/a[b](c).txt",
                "a[b](c).txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                13,
                "/data/needle/archive.dat",
                "archive.dat",
                "/data/needle",
                "dat",
                Some(12),
                false,
            ),
            (
                14,
                "/data/abcXYZdef.txt",
                "abcXYZdef.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                15,
                "/data/abcdef.txt",
                "abcdef.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                16,
                "/data/ſong.txt",
                "ſong.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                17,
                "/data/a\nb.txt",
                "a\nb.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
            (
                18,
                "/data/say\"hi.txt",
                "say\"hi.txt",
                "/data",
                "txt",
                Some(12),
                false,
            ),
        ];
        for (id, path, name, parent, ext, size, is_dir) in rows {
            conn.execute(
                "INSERT INTO files (id,path,name,parent,extension,name_fold,path_fold,parent_fold,
                    size,modified,created,is_dir) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    id,
                    path,
                    name,
                    parent,
                    ext,
                    name.to_lowercase(),
                    path.to_lowercase(),
                    parent.to_lowercase(),
                    size,
                    Option::<i64>::None,
                    Option::<i64>::None,
                    is_dir
                ],
            )
            .unwrap();
        }
        for (id, day, hour, minute, second) in [
            (1, 13, 0, 0, 0),
            (2, 13, 23, 59, 59),
            (3, 14, 0, 0, 0),
            (4, 12, 12, 0, 0),
        ] {
            let modified = Local
                .with_ymd_and_hms(2026, 11, day, hour, minute, second)
                .earliest()
                .unwrap()
                .timestamp();
            let created = Local
                .with_ymd_and_hms(2026, 10, day, hour, minute, second)
                .earliest()
                .unwrap()
                .timestamp();
            conn.execute(
                "UPDATE files SET modified=?, created=? WHERE id=?",
                params![modified, created, id],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO file_fts(file_fts) VALUES ('rebuild')", [])
            .unwrap();
        register_functions(&conn).unwrap();
        conn
    }

    fn select(conn: &Connection, text: &str, options: SearchOptions) -> Vec<i64> {
        let query = compile(text, &options).unwrap_or_else(|error| panic!("{text:?}: {error:#}"));
        conn.prepare(&format!(
            "SELECT f.id FROM files f WHERE {} ORDER BY f.id",
            query.where_sql
        ))
        .unwrap()
        .query_map(params_from_iter(query.params), |row| row.get(0))
        .unwrap_or_else(|error| panic!("{text:?}: {error:#}"))
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn ids(text: &str) -> Vec<i64> {
        select(&fixture(), text, SearchOptions::default())
    }

    #[test]
    fn empty_query_matches_all_records() {
        assert_eq!(ids("  "), (1..=18).collect::<Vec<_>>());
    }

    #[test]
    fn literals_are_unicode_folded_substrings() {
        assert_eq!(ids("REPORT"), vec![1, 2, 3]);
        assert_eq!(ids("İSTAN"), vec![9]);
        assert_eq!(ids("%_"), vec![11]);
    }

    #[test]
    fn boolean_precedence_and_grouping_preserve_parameter_order() {
        assert_eq!(ids("report !ext:csv | ext:jpg"), vec![1, 2, 4]);
        assert_eq!(ids("(report OR photo) AND NOT ext:csv"), vec![1, 2, 4]);
        assert_eq!(ids("!(report | folder:) ext:gz"), vec![7]);
    }

    #[test]
    fn quoted_text_preserves_spaces_punctuation_and_backslashes() {
        assert_eq!(ids(r#""report final""#), vec![2]);
        assert_eq!(ids(r#""a[b](c)""#), vec![12]);
        assert_eq!(ids(r#""C:\Program Files\App\""#), vec![5]);
        assert_eq!(ids(r#""say""hi""#), vec![18]);
    }

    #[test]
    fn drive_letters_are_paths_not_unknown_functions() {
        assert_eq!(ids("C:"), vec![5]);
        assert_eq!(ids(r"C:\Program*"), vec![5]);
        assert_eq!(ids("/data/needle"), vec![13]);
    }

    #[test]
    fn wildcards_match_whole_names_and_one_unicode_character() {
        assert_eq!(ids("report*.txt"), vec![1, 2]);
        assert_eq!(ids("a?b.txt"), vec![17]);
        assert!(ids("report?").is_empty());
        assert_eq!(ids("İ?TANBUL.txt"), vec![9]);
        assert_eq!(ids("?STANBUL.txt"), vec![9]);
        assert_eq!(ids("*song*"), vec![6, 16]);
    }

    #[test]
    fn case_scope_and_options_are_case_sensitive() {
        assert_eq!(ids("case:Report"), vec![1]);
        assert_eq!(ids("case:(Report | photo)"), vec![1, 4]);
        assert_eq!(
            select(
                &fixture(),
                "Report",
                SearchOptions {
                    case_sensitive: true,
                    ..Default::default()
                }
            ),
            vec![1]
        );
    }

    #[test]
    fn case_sensitive_trigrams_do_not_lose_contextual_sigma_matches() {
        assert_eq!(ids("case:ΑΣΟΣ"), vec![10]);
    }

    #[test]
    fn whole_word_and_path_options_change_actual_matches() {
        assert_eq!(
            select(
                &fixture(),
                "report",
                SearchOptions {
                    whole_word: true,
                    ..Default::default()
                }
            ),
            vec![1, 2]
        );
        assert_eq!(
            select(
                &fixture(),
                "needle",
                SearchOptions {
                    match_path: true,
                    ..Default::default()
                }
            ),
            vec![13]
        );
        assert!(ids("needle").is_empty());
    }

    #[test]
    fn exact_predicate_removes_trigram_false_positives() {
        assert_eq!(ids("abcdef"), vec![15]);
        let conn = fixture();
        let query = compile("abcdef", &SearchOptions::default()).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT f.id FROM files f WHERE {}",
                query.where_sql
            ))
            .unwrap()
            .query_map(params_from_iter(query.params), |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|line| line.contains("VIRTUAL TABLE INDEX")),
            "{plan:?}"
        );
    }

    #[test]
    fn selective_trigram_candidates_include_the_suffix_of_shared_prefixes() {
        let conn = fixture();
        for id in 0..1_000 {
            let name = format!("invoice_{id:08}.txt");
            let path = format!("/benchmark/project_{id:04}/{name}");
            conn.execute(
                "INSERT INTO files (id,path,name,parent,extension,name_fold,path_fold,parent_fold,is_dir)
                 VALUES (?, ?, ?, '/benchmark', 'txt', ?, ?, '/benchmark', 0)",
                params![1_000 + id, path, name, name, path],
            ).unwrap();
        }
        conn.execute("INSERT INTO file_fts(file_fts) VALUES ('rebuild')", [])
            .unwrap();
        for (text, match_path, expected) in [
            ("invoice_00000100", false, 1_100),
            ("project_0042", true, 1_042),
        ] {
            let options = SearchOptions {
                match_path,
                ..Default::default()
            };
            let query = compile(text, &options).unwrap();
            // Measure the actual index candidate set, not wall-clock timing:
            // using only the common prefix admits all 1,000 records here.
            let candidates: i64 = conn
                .query_row(
                    "SELECT count(*) FROM file_fts WHERE file_fts MATCH ?",
                    params![query.params[0]],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(candidates < 32, "{text:?} admitted {candidates} candidates");
            assert_eq!(select(&conn, text, options), vec![expected]);
        }
    }

    #[test]
    fn explicit_regex_composes_with_metadata_and_preserves_regex_escapes() {
        assert_eq!(ids(r#"regex:"^report( final)?\.txt$" ext:txt"#), vec![1, 2]);
        assert_eq!(ids(r#"case:regex:"^Report\.TXT$""#), vec![1]);
    }

    #[test]
    fn regex_mode_uses_the_complete_expression_and_unicode_case_folding() {
        let options = SearchOptions {
            regex: true,
            ..Default::default()
        };
        assert_eq!(
            select(&fixture(), r"^report( final)?\.txt$|^photo\.jpg$", options),
            vec![1, 2, 4]
        );
        assert_eq!(select(&fixture(), "song", options), vec![6, 16]);
    }

    #[test]
    fn extension_lists_accept_dots_and_exclude_nonmatching_types() {
        assert_eq!(ids("ext:.JPG;gz"), vec![4, 7]);
        assert_eq!(ids("file: ext:csv"), vec![3]);
        assert_eq!(ids("folder:"), vec![8]);
        assert_eq!(ids("folder:docs"), vec![8]);
    }

    #[test]
    fn type_presets_filter_by_extension() {
        for (query, expected) in [
            ("audio:", vec![6]),
            ("pic:", vec![4]),
            ("exe:", vec![5]),
            ("compressed:", vec![7]),
            ("video:", vec![]),
        ] {
            assert_eq!(ids(query), expected, "{query}");
        }
        assert_eq!(ids("doc:report"), vec![1, 2, 3]);
    }

    #[test]
    fn path_and_parent_have_distinct_descendant_semantics() {
        assert_eq!(ids("path:/data/needle"), vec![13]);
        assert_eq!(ids("parent:/data/needle/"), vec![13]);
        assert!(!ids("parent:/data").contains(&13));
        assert_eq!(ids(r#"parent:"c:\program files\app\""#), vec![5]);
    }

    #[test]
    fn sizes_support_decimal_binary_units_ranges_and_comparators() {
        assert_eq!(ids("size:1.5KB..2KiB"), vec![1, 2, 5]);
        assert_eq!(ids("size:>2KB"), vec![2, 4, 7]);
        assert_eq!(ids("size:=1MiB"), vec![4]);
        assert_eq!(ids("size:0"), vec![6]);
        assert!(!ids("size:>=0").contains(&8));
        assert_eq!(ids("size:4KB.."), vec![4, 7]);
        assert_eq!(ids("size:<=0"), vec![6]);
        assert_eq!(ids("size:!=1KB report"), vec![1, 2]);
    }

    #[test]
    fn date_filters_use_entire_local_calendar_days_and_inclusive_ranges() {
        assert_eq!(ids("dm:2026-11-13"), vec![1, 2]);
        assert_eq!(ids("dm:2026-11-12..2026-11-13"), vec![1, 2, 4]);
        assert_eq!(ids("dm:>2026-11-13"), vec![3]);
        assert_eq!(ids("dm:<=2026-11-13"), vec![1, 2, 4]);
        assert_eq!(ids("date-created:2026-10-13"), vec![1, 2]);
        assert_eq!(ids("date-modified:2026-11"), vec![1, 2, 3, 4]);
    }

    #[test]
    fn negative_metadata_predicates_include_unknown_values_without_faking_zero() {
        assert!(ids("!size:>0").contains(&8));
        assert!(!ids("size:0").contains(&8));
        assert!(ids("!dm:2026").contains(&8));
    }

    #[test]
    fn injection_text_remains_data_and_never_changes_sql() {
        let conn = fixture();
        assert!(select(&conn, r#""' OR 1=1 --""#, Default::default()).is_empty());
        assert!(select(&conn, r#"ext:"txt') OR 1=1 --""#, Default::default()).is_empty());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM files", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            18
        );
    }

    #[test]
    fn malformed_input_and_unsupported_content_report_errors() {
        for text in [
            "unknown:value",
            "size:",
            "size:>",
            "size:abc",
            "size:1..0",
            "size:9223372036854775808",
            "size:NaN",
            "size:1.1B",
            "dm:2026-02-30",
            "dm:2026-11-14..2026-11-13",
            "ext:",
            "(",
            "foo |",
            "!",
            "foo OR OR bar",
            "AND foo",
            "foo)",
            "\"unclosed",
            r#"regex:"[""#,
            "content:secret",
        ] {
            assert!(
                compile(text, &Default::default()).is_err(),
                "accepted {text:?}"
            );
        }
        let error = compile("content:secret", &Default::default())
            .unwrap_err()
            .to_string();
        assert!(
            error.to_ascii_lowercase().contains("not supported"),
            "{error}"
        );
    }

    #[test]
    fn quoted_boolean_keywords_and_trigram_punctuation_are_literal() {
        assert_eq!(ids(r#""OR""#), vec![1, 2, 3]);
        assert_eq!(ids(r#""y""h""#), vec![18]);
        assert_eq!(ids("\"a\nb\""), vec![17]);
    }

    #[test]
    fn relative_dates_use_local_calendar_day() {
        let conn = fixture();
        let today = Local::now().date_naive();
        let midday = Local
            .from_local_datetime(&today.and_hms_opt(12, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp();
        conn.execute("UPDATE files SET modified=NULL", []).unwrap();
        conn.execute("UPDATE files SET modified=? WHERE id=1", [midday])
            .unwrap();
        assert_eq!(select(&conn, "dm:today", Default::default()), vec![1]);
        assert!(select(&conn, "dm:yesterday", Default::default()).is_empty());
    }

    #[test]
    fn calendar_day_includes_midday_when_dst_skips_midnight() {
        // Run this case with TZ=America/Sao_Paulo, where 2018-11-04
        // started at 01:00. It also remains valid under other local timezones.
        let conn = fixture();
        let midday = Local
            .with_ymd_and_hms(2018, 11, 4, 12, 0, 0)
            .earliest()
            .unwrap()
            .timestamp();
        conn.execute("UPDATE files SET modified=NULL", []).unwrap();
        conn.execute("UPDATE files SET modified=? WHERE id=1", [midday])
            .unwrap();
        assert_eq!(select(&conn, "dm:2018-11-04", Default::default()), vec![1]);
    }

    #[test]
    fn indexed_unicode_candidates_agree_with_authoritative_substring_predicate() {
        let conn = fixture();
        let names = [
            "ⅢΣŒ🙂İe\u{301}é한글.txt",
            "AΣΟΣΑſK.txt",
            "é\"\n🙂漢字.txt",
            "aλ中🙂ΣΟΣΑλmiddleEnding.txt",
        ];
        for (offset, name) in names.iter().enumerate() {
            conn.execute(
                "INSERT INTO files (id,path,name,parent,extension,name_fold,path_fold,parent_fold,is_dir)
                 VALUES (?, ?, ?, '/data', 'txt', ?, ?, '/data', 0)",
                params![100 + offset as i64, format!("/data/{name}"), name, name.to_lowercase(), format!("/data/{name}").to_lowercase()],
            ).unwrap();
        }
        conn.execute("INSERT INTO file_fts(file_fts) VALUES ('rebuild')", [])
            .unwrap();
        for name in names {
            let characters = name.chars().collect::<Vec<_>>();
            for width in [3, 4, 5, 6, 9, 12, 16] {
                for window in characters.windows(width) {
                    let term = window.iter().collect::<String>();
                    for case_sensitive in [false, true] {
                        let (column, value, prefix) = if case_sensitive {
                            ("name", term.clone(), "case:")
                        } else {
                            ("name_fold", term.to_lowercase(), "")
                        };
                        let expected: Vec<i64> = conn
                            .prepare(&format!(
                                "SELECT id FROM files WHERE instr({column}, ?) > 0 ORDER BY id"
                            ))
                            .unwrap()
                            .query_map([value], |row| row.get(0))
                            .unwrap()
                            .collect::<rusqlite::Result<_>>()
                            .unwrap();
                        let query = format!("{prefix}\"{}\"", term.replace('"', "\"\""));
                        assert_eq!(
                            select(&conn, &query, Default::default()),
                            expected,
                            "{query:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn deeply_nested_and_oversized_queries_fail_without_panicking() {
        let nested = format!("{}name{}", "(".repeat(1000), ")".repeat(1000));
        assert!(compile(&nested, &Default::default()).is_err());
        assert!(compile(&"a".repeat(100_000), &Default::default()).is_err());
    }
}

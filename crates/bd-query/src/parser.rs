//! The AST and a recursive-descent parser for it.
//!
//! ```text
//! query      := or_expr
//! or_expr    := and_expr ( "OR" and_expr )*
//! and_expr   := not_expr ( "AND" not_expr )*
//! not_expr   := "NOT" not_expr | primary        -- right-associative
//! primary    := "(" or_expr ")" | comparison
//! comparison := IDENT op value
//! op         := "=" | "!=" | "<" | "<=" | ">" | ">="
//! value      := IDENT | STRING | NUMBER | DURATION
//! ```
//!
//! Parsing is where *every* error in this crate is raised. Field names,
//! operators, and values are all resolved and validated here, so the AST that
//! comes out is total: [`crate::Query::matches`] and the filter builders cannot
//! fail, and therefore have no error paths for a caller to mishandle.

use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveDateTime, TimeDelta, TimeZone, Utc};

use crate::error::{Error, Result};
use crate::lexer::{Lexer, Tok, Token};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Id,
    Title,
    Description,
    Status,
    Priority,
    Type,
    Assignee,
    Owner,
    Created,
    Updated,
    Closed,
    Started,
    Label,
    Pinned,
    Ephemeral,
    Template,
    Spec,
    Parent,
    Notes,
    HasMetadataKey,
}

impl Field {
    /// The canonical name, used in error messages. Aliases collapse onto it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Field::Id => "id",
            Field::Title => "title",
            Field::Description => "description",
            Field::Status => "status",
            Field::Priority => "priority",
            Field::Type => "type",
            Field::Assignee => "assignee",
            Field::Owner => "owner",
            Field::Created => "created",
            Field::Updated => "updated",
            Field::Closed => "closed",
            Field::Started => "started",
            Field::Label => "label",
            Field::Pinned => "pinned",
            Field::Ephemeral => "ephemeral",
            Field::Template => "template",
            Field::Spec => "spec",
            Field::Parent => "parent",
            Field::Notes => "notes",
            Field::HasMetadataKey => "has_metadata_key",
        }
    }

    fn parse(name: &str) -> Result<Field> {
        Ok(match name.to_ascii_lowercase().as_str() {
            "id" => Field::Id,
            "title" => Field::Title,
            "description" | "desc" => Field::Description,
            "status" => Field::Status,
            "priority" => Field::Priority,
            "type" => Field::Type,
            "assignee" => Field::Assignee,
            "owner" => Field::Owner,
            "created" | "created_at" => Field::Created,
            "updated" | "updated_at" => Field::Updated,
            "closed" | "closed_at" => Field::Closed,
            "started" | "started_at" => Field::Started,
            "label" | "labels" => Field::Label,
            "pinned" => Field::Pinned,
            "ephemeral" => Field::Ephemeral,
            "template" => Field::Template,
            "spec" | "spec_id" => Field::Spec,
            "parent" => Field::Parent,
            "notes" => Field::Notes,
            "has_metadata_key" => Field::HasMetadataKey,
            _ => return Err(Error::UnknownField(name.to_string())),
        })
    }

    fn is_date(self) -> bool {
        matches!(
            self,
            Field::Created | Field::Updated | Field::Closed | Field::Started
        )
    }

    fn is_bool(self) -> bool {
        matches!(self, Field::Pinned | Field::Ephemeral | Field::Template)
    }
}

/// A comparison's value, already resolved against its field. Durations are
/// absolute instants by the time they land here — the query's notion of "now"
/// is fixed at parse time so that the SQL filter and the in-memory predicate
/// cannot disagree about it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Text(String),
    Int(i32),
    Time(DateTime<Utc>),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cmp {
    pub field: Field,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Cmp(Cmp),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
}

pub(crate) struct Parser {
    lex: Lexer,
    cur: Token,
    now: DateTime<Utc>,
}

impl Parser {
    pub(crate) fn new(input: &str, now: DateTime<Utc>) -> Result<Self> {
        let mut lex = Lexer::new(input);
        let cur = lex.next_token()?;
        Ok(Parser { lex, cur, now })
    }

    pub(crate) fn parse(mut self) -> Result<Node> {
        if self.cur.kind == Tok::Eof {
            return Err(Error::EmptyQuery);
        }
        let node = self.parse_or()?;
        if self.cur.kind != Tok::Eof {
            return Err(self.unexpected("end of query"));
        }
        Ok(node)
    }

    fn advance(&mut self) -> Result<()> {
        self.cur = self.lex.next_token()?;
        Ok(())
    }

    fn unexpected(&self, expected: &str) -> Error {
        if self.cur.kind == Tok::Eof {
            Error::UnexpectedEof(expected.to_string())
        } else {
            Error::UnexpectedToken {
                found: self.cur.text.clone(),
                pos: self.cur.pos,
                expected: expected.to_string(),
            }
        }
    }

    fn parse_or(&mut self) -> Result<Node> {
        let mut left = self.parse_and()?;
        while self.cur.kind == Tok::Or {
            self.advance()?;
            let right = self.parse_and()?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Node> {
        let mut left = self.parse_not()?;
        while self.cur.kind == Tok::And {
            self.advance()?;
            let right = self.parse_not()?;
            left = Node::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Node> {
        if self.cur.kind == Tok::Not {
            self.advance()?;
            // Right-associative: `NOT NOT x` is `NOT (NOT x)`.
            let operand = self.parse_not()?;
            return Ok(Node::Not(Box::new(operand)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Node> {
        if self.cur.kind == Tok::LParen {
            self.advance()?;
            let node = self.parse_or()?;
            if self.cur.kind != Tok::RParen {
                return Err(self.unexpected("')'"));
            }
            self.advance()?;
            return Ok(node);
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Node> {
        if self.cur.kind != Tok::Ident {
            return Err(self.unexpected("a field name"));
        }
        let field = Field::parse(&self.cur.text)?;
        self.advance()?;

        let op = match self.cur.kind {
            Tok::Eq => Op::Eq,
            Tok::Ne => Op::Ne,
            Tok::Lt => Op::Lt,
            Tok::Le => Op::Le,
            Tok::Gt => Op::Gt,
            Tok::Ge => Op::Ge,
            _ => return Err(self.unexpected("a comparison operator")),
        };
        self.advance()?;

        if !matches!(self.cur.kind, Tok::Ident | Tok::Str | Tok::Num | Tok::Duration) {
            return Err(self.unexpected("a value"));
        }
        let tok = self.cur.clone();
        self.advance()?;

        let value = self.resolve(field, op, &tok)?;
        Ok(Node::Cmp(Cmp { field, op, value }))
    }

    /// Type-check a comparison: which operators the field admits, and what its
    /// value means.
    fn resolve(&self, field: Field, op: Op, tok: &Token) -> Result<Value> {
        let bad_op = || Error::BadOperator {
            op: op.as_str().to_string(),
            field: field.as_str().to_string(),
        };
        let invalid = |reason: &str| Error::InvalidValue {
            field: field.as_str().to_string(),
            value: tok.text.clone(),
            reason: reason.to_string(),
        };

        if field == Field::Priority {
            let n: i32 = tok
                .text
                .parse()
                .map_err(|_| invalid("expected a number 0-4"))?;
            if !(0..=4).contains(&n) {
                return Err(invalid("priority must be between 0 (critical) and 4 (trivial)"));
            }
            return Ok(Value::Int(n));
        }

        if field.is_date() {
            return Ok(Value::Time(self.resolve_time(tok, &invalid)?));
        }

        if field.is_bool() {
            if !matches!(op, Op::Eq | Op::Ne) {
                return Err(bad_op());
            }
            return match tok.text.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Ok(Value::Bool(true)),
                "false" | "no" | "0" => Ok(Value::Bool(false)),
                _ => Err(invalid("expected true/false, yes/no, or 1/0")),
            };
        }

        // Everything else is a string field, and ordering a string is not a
        // question this language answers.
        if !matches!(op, Op::Eq | Op::Ne) {
            return Err(bad_op());
        }
        // `Status::from` does *not* lowercase, so `status=OPEN` would become the
        // custom status "OPEN" rather than `Status::Open`. Normalize the two
        // enum-valued fields here, once, rather than at every use site.
        if matches!(field, Field::Status | Field::Type) {
            return Ok(Value::Text(tok.text.to_ascii_lowercase()));
        }
        Ok(Value::Text(tok.text.clone()))
    }

    fn resolve_time(&self, tok: &Token, invalid: &dyn Fn(&str) -> Error) -> Result<DateTime<Utc>> {
        if tok.kind == Tok::Duration {
            return self.duration_ago(&tok.text, invalid);
        }
        let s = tok.text.trim();
        match s.to_ascii_lowercase().as_str() {
            "now" => return Ok(self.now),
            "today" => return Ok(midnight(self.now)),
            "yesterday" => {
                return midnight(self.now)
                    .checked_sub_signed(TimeDelta::days(1))
                    .ok_or_else(|| invalid("date out of range"));
            }
            _ => {}
        }
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight exists")));
        }
        if let Ok(t) = DateTime::parse_from_rfc3339(s) {
            return Ok(t.with_timezone(&Utc));
        }
        // Accept a bare local-looking timestamp, read as UTC: this language has
        // no timezone of its own, and silently applying the machine's would make
        // the same query mean different things on different machines.
        if let Ok(t) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        {
            return Ok(Utc.from_utc_datetime(&t));
        }
        Err(invalid(
            "expected a duration (7d, 24h), a date (2026-07-14), an RFC 3339 timestamp, or now/today/yesterday",
        ))
    }

    /// A duration on a date field is always *backwards*: `created>7d` asks for
    /// issues created within the last 7 days, i.e. `created_at > now - 7d`. The
    /// duration names the far edge of the window, never a point in the future,
    /// which is why an explicitly negative duration is rejected rather than
    /// quietly reflected.
    fn duration_ago(&self, text: &str, invalid: &dyn Fn(&str) -> Error) -> Result<DateTime<Utc>> {
        let body = match text.strip_prefix('+') {
            Some(rest) => rest,
            None if text.starts_with('-') => {
                return Err(invalid(
                    "a duration already means 'ago'; drop the minus sign",
                ));
            }
            None => text,
        };
        let (digits, unit) = body.split_at(body.len() - 1);
        let n: i64 = digits
            .parse()
            .map_err(|_| invalid("expected a duration like 7d or 24h"))?;

        let out = match unit.to_ascii_lowercase().as_str() {
            "h" => self.now.checked_sub_signed(TimeDelta::hours(n)),
            "d" => self.now.checked_sub_signed(TimeDelta::days(n)),
            "w" => self.now.checked_sub_signed(TimeDelta::weeks(n)),
            // Months and years are calendar arithmetic, not fixed spans: "1m
            // ago" on Mar 31 is Feb 28, not Mar 3.
            "m" => u32::try_from(n)
                .ok()
                .and_then(|n| self.now.checked_sub_months(Months::new(n))),
            "y" => u32::try_from(n)
                .ok()
                .and_then(|n| n.checked_mul(12))
                .and_then(|n| self.now.checked_sub_months(Months::new(n))),
            _ => return Err(invalid("unknown duration unit; use h, d, w, m, or y")),
        };
        out.ok_or_else(|| invalid("duration reaches outside the representable range"))
    }
}

fn midnight(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(t.year(), t.month(), t.day(), 0, 0, 0)
        .single()
        .expect("UTC midnight is unambiguous")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0).unwrap()
    }

    fn parse(input: &str) -> Result<Node> {
        Parser::new(input, at())?.parse()
    }

    /// A compact rendering of the tree, so precedence assertions read as the
    /// shape they are checking.
    fn shape(n: &Node) -> String {
        match n {
            Node::Cmp(c) => format!("{}{}{:?}", c.field.as_str(), c.op.as_str(), c.value),
            Node::And(l, r) => format!("({} AND {})", shape(l), shape(r)),
            Node::Or(l, r) => format!("({} OR {})", shape(l), shape(r)),
            Node::Not(x) => format!("NOT {}", shape(x)),
        }
    }

    fn s(input: &str) -> String {
        shape(&parse(input).unwrap())
    }

    #[test]
    fn and_binds_tighter_than_or() {
        assert_eq!(
            s("status=open OR type=bug AND priority=1"),
            r#"(status=Text("open") OR (type=Text("bug") AND priority=Int(1)))"#
        );
    }

    #[test]
    fn parens_override_precedence() {
        assert_eq!(
            s("(status=open OR type=bug) AND priority=1"),
            r#"((status=Text("open") OR type=Text("bug")) AND priority=Int(1))"#
        );
    }

    #[test]
    fn or_and_and_are_left_associative() {
        assert_eq!(
            s("label=a OR label=b OR label=c"),
            r#"((label=Text("a") OR label=Text("b")) OR label=Text("c"))"#
        );
        assert_eq!(
            s("label=a AND label=b AND label=c"),
            r#"((label=Text("a") AND label=Text("b")) AND label=Text("c"))"#
        );
    }

    #[test]
    fn not_is_right_associative_and_binds_tighter_than_and() {
        assert_eq!(
            s("NOT NOT status=open"),
            r#"NOT NOT status=Text("open")"#
        );
        // NOT takes only the comparison to its right, not the whole AND.
        assert_eq!(
            s("NOT status=closed AND priority=0"),
            r#"(NOT status=Text("closed") AND priority=Int(0))"#
        );
        assert_eq!(
            s("NOT (status=closed AND priority=0)"),
            r#"NOT (status=Text("closed") AND priority=Int(0))"#
        );
    }

    #[test]
    fn aliases_collapse_onto_one_field() {
        assert_eq!(s("desc=x"), s("description=x"));
        assert_eq!(s("labels=x"), s("label=x"));
        assert_eq!(s("created_at>7d"), s("created>7d"));
        assert_eq!(s("spec_id=x"), s("spec=x"));
        // Field names are case-insensitive; values are not (except status/type).
        assert_eq!(s("STATUS=open"), s("status=open"));
    }

    #[test]
    fn status_and_type_values_are_lowercased() {
        // Status::from does not normalize case, so an un-lowercased "OPEN" would
        // become Status::Custom("OPEN") and match nothing.
        assert_eq!(s("status=OPEN"), s("status=open"));
        assert_eq!(s("type=BUG"), s("type=bug"));
        // A label, by contrast, is whatever the user typed.
        assert_ne!(s("label=Infra"), s("label=infra"));
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert_eq!(
            parse("bogus=1").unwrap_err(),
            Error::UnknownField("bogus".into())
        );
        // Upstream accepts `mol_type` and `metadata.<key>`; this port does not,
        // and says so rather than silently matching nothing.
        assert_eq!(
            parse("mol_type=swarm").unwrap_err(),
            Error::UnknownField("mol_type".into())
        );
    }

    #[test]
    fn bad_operators_are_rejected_at_parse_time() {
        assert_eq!(
            parse("status<open").unwrap_err(),
            Error::BadOperator {
                op: "<".into(),
                field: "status".into()
            }
        );
        assert_eq!(
            parse("assignee>=bob").unwrap_err(),
            Error::BadOperator {
                op: ">=".into(),
                field: "assignee".into()
            }
        );
        // Dates and priorities admit the full set.
        assert!(parse("created<=2026-01-01").is_ok());
        assert!(parse("priority>=2").is_ok());
    }

    #[test]
    fn malformed_queries_are_rejected() {
        assert_eq!(parse("").unwrap_err(), Error::EmptyQuery);
        assert_eq!(parse("   ").unwrap_err(), Error::EmptyQuery);
        assert!(matches!(parse("status="), Err(Error::UnexpectedEof(_))));
        assert!(matches!(parse("status"), Err(Error::UnexpectedEof(_))));
        assert!(matches!(parse("=open"), Err(Error::UnexpectedToken { .. })));
        assert!(matches!(
            parse("(status=open"),
            Err(Error::UnexpectedEof(_))
        ));
        assert!(matches!(
            parse("status=open extra=1"),
            Err(Error::UnexpectedToken { .. })
        ));
        assert!(matches!(
            parse("status=open AND"),
            Err(Error::UnexpectedEof(_))
        ));
        assert!(matches!(parse("NOT"), Err(Error::UnexpectedEof(_))));
    }

    #[test]
    fn priority_values_are_range_checked() {
        assert!(matches!(parse("priority=5"), Err(Error::InvalidValue { .. })));
        assert!(matches!(
            parse("priority=-1"),
            Err(Error::InvalidValue { .. })
        ));
        assert!(matches!(
            parse("priority=high"),
            Err(Error::InvalidValue { .. })
        ));
        assert!(parse("priority=0").is_ok());
        assert!(parse("priority=4").is_ok());
    }

    #[test]
    fn duration_resolves_backwards_from_now() {
        // `created>7d` = "created within the last 7 days" = created_at > now-7d.
        let Node::Cmp(c) = parse("created>7d").unwrap() else {
            panic!("expected a comparison")
        };
        assert_eq!(c.value, Value::Time(at() - TimeDelta::days(7)));

        let Node::Cmp(c) = parse("updated>24h").unwrap() else {
            panic!("expected a comparison")
        };
        assert_eq!(c.value, Value::Time(at() - TimeDelta::hours(24)));

        // Calendar units are calendar arithmetic.
        let Node::Cmp(c) = parse("created>1m").unwrap() else {
            panic!("expected a comparison")
        };
        assert_eq!(c.value, Value::Time(Utc.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap()));
        let Node::Cmp(c) = parse("created>1y").unwrap() else {
            panic!("expected a comparison")
        };
        assert_eq!(c.value, Value::Time(Utc.with_ymd_and_hms(2025, 7, 14, 12, 0, 0).unwrap()));
    }

    #[test]
    fn negative_duration_is_rejected() {
        // `-7d` would read as "7 days into the future", which no date field
        // means. `+7d` is accepted as a synonym for `7d`.
        assert!(matches!(
            parse("created>-7d"),
            Err(Error::InvalidValue { .. })
        ));
        assert_eq!(s("created>+7d"), s("created>7d"));
    }

    #[test]
    fn absolute_dates_parse() {
        let Node::Cmp(c) = parse("created<2026-01-15").unwrap() else {
            panic!("expected a comparison")
        };
        assert_eq!(
            c.value,
            Value::Time(Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap())
        );
        let Node::Cmp(c) = parse("created<2026-01-15T10:30:00Z").unwrap() else {
            panic!("expected a comparison")
        };
        assert_eq!(
            c.value,
            Value::Time(Utc.with_ymd_and_hms(2026, 1, 15, 10, 30, 0).unwrap())
        );
        let Node::Cmp(c) = parse("created>yesterday").unwrap() else {
            panic!("expected a comparison")
        };
        assert_eq!(
            c.value,
            Value::Time(Utc.with_ymd_and_hms(2026, 7, 13, 0, 0, 0).unwrap())
        );
        assert!(matches!(
            parse("created>nonsense"),
            Err(Error::InvalidValue { .. })
        ));
    }

    #[test]
    fn bool_fields_take_the_usual_spellings() {
        for t in ["true", "yes", "1", "TRUE"] {
            let Node::Cmp(c) = parse(&format!("pinned={t}")).unwrap() else {
                panic!("expected a comparison")
            };
            assert_eq!(c.value, Value::Bool(true), "{t}");
        }
        for f in ["false", "no", "0"] {
            let Node::Cmp(c) = parse(&format!("ephemeral={f}")).unwrap() else {
                panic!("expected a comparison")
            };
            assert_eq!(c.value, Value::Bool(false), "{f}");
        }
        assert!(matches!(
            parse("template=maybe"),
            Err(Error::InvalidValue { .. })
        ));
    }
}

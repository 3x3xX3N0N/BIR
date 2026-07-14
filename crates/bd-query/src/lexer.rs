//! Hand-written lexer for the filter language.
//!
//! Positions are *character* offsets, not byte offsets, so an error on a query
//! containing non-ASCII text points at the character the user sees.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tok {
    Eof,
    Ident,
    Str,
    Num,
    Duration,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    LParen,
    RParen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: Tok,
    /// The token's text. For strings this is the *decoded* value (escapes
    /// applied, quotes stripped); for everything else it is the raw lexeme.
    pub text: String,
    pub pos: usize,
}

pub(crate) struct Lexer {
    src: Vec<char>,
    pos: usize,
}

/// A bare identifier may start with a letter or `_` only. Anything digit-led
/// goes through the number path first — see [`Lexer::read_number_or_duration`].
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// `:` is deliberately an identifier character: namespaced labels like
/// `gt:merge-request` are the common case, and forcing quotes on them would
/// make the most-typed queries the ugliest ones. `-` and `.` are in for the
/// same reason (`in_progress`, `bd-123`, `v1.2.3`).
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')
}

fn is_duration_suffix(c: char) -> bool {
    matches!(
        c,
        'h' | 'd' | 'w' | 'm' | 'y' | 'H' | 'D' | 'W' | 'M' | 'Y'
    )
}

impl Lexer {
    pub(crate) fn new(input: &str) -> Self {
        Lexer {
            src: input.chars().collect(),
            pos: 0,
        }
    }

    fn at(&self, i: usize) -> Option<char> {
        self.src.get(i).copied()
    }

    fn peek(&self) -> Option<char> {
        self.at(self.pos)
    }

    pub(crate) fn next_token(&mut self) -> Result<Token> {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }

        let start = self.pos;
        let Some(c) = self.peek() else {
            return Ok(Token {
                kind: Tok::Eof,
                text: String::new(),
                pos: start,
            });
        };

        let one = |kind: Tok, text: &str| Token {
            kind,
            text: text.to_string(),
            pos: start,
        };

        match c {
            '(' => {
                self.pos += 1;
                Ok(one(Tok::LParen, "("))
            }
            ')' => {
                self.pos += 1;
                Ok(one(Tok::RParen, ")"))
            }
            '=' => {
                self.pos += 1;
                Ok(one(Tok::Eq, "="))
            }
            '!' => {
                if self.at(start + 1) == Some('=') {
                    self.pos += 2;
                    Ok(one(Tok::Ne, "!="))
                } else {
                    // Bare `!` is never valid: negation is spelled `NOT`.
                    Err(Error::UnexpectedChar { ch: '!', pos: start })
                }
            }
            '<' | '>' => {
                let eq = self.at(start + 1) == Some('=');
                self.pos += if eq { 2 } else { 1 };
                Ok(match (c, eq) {
                    ('<', false) => one(Tok::Lt, "<"),
                    ('<', true) => one(Tok::Le, "<="),
                    ('>', false) => one(Tok::Gt, ">"),
                    _ => one(Tok::Ge, ">="),
                })
            }
            '"' | '\'' => self.read_string(c, start),
            _ if c.is_ascii_digit() || c == '-' || c == '+' => self.read_number_or_duration(start),
            _ if is_ident_start(c) => Ok(self.read_ident(start)),
            _ => Err(Error::UnexpectedChar { ch: c, pos: start }),
        }
    }

    fn read_string(&mut self, quote: char, start: usize) -> Result<Token> {
        let mut out = String::new();
        let mut i = start + 1; // skip the opening quote
        loop {
            let Some(c) = self.at(i) else {
                return Err(Error::UnterminatedString(start));
            };
            i += 1;
            if c == quote {
                self.pos = i;
                return Ok(Token {
                    kind: Tok::Str,
                    text: out,
                    pos: start,
                });
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            // A backslash at the very end leaves the string unterminated —
            // report the string, not the escape, since that is what the user
            // has to fix.
            let Some(esc) = self.at(i) else {
                return Err(Error::UnterminatedString(start));
            };
            i += 1;
            out.push(match esc {
                'n' => '\n',
                't' => '\t',
                // An unknown escape yields the character itself, so `\x` is
                // `x`. Erroring here would break paste-in-a-regex workflows for
                // no benefit.
                other => other,
            });
        }
    }

    /// Numbers, durations (`7d`, `24h`), and the digit-led identifiers that
    /// masquerade as them.
    fn read_number_or_duration(&mut self, start: usize) -> Result<Token> {
        let mut i = start;
        let signed = matches!(self.at(i), Some('-') | Some('+'));
        if signed {
            i += 1;
        }
        if !self.at(i).is_some_and(|c| c.is_ascii_digit()) {
            // `-foo`: a sign with no digits. Not an identifier — see below.
            return Err(Error::UnexpectedChar {
                ch: self.at(start).unwrap_or('-'),
                pos: start,
            });
        }
        while self.at(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }

        if let Some(c) = self.at(i) {
            // Commit to a duration only when the suffix stands alone. `7d` is a
            // duration; `7day` is not, and must not silently become one.
            if is_duration_suffix(c) && !self.at(i + 1).is_some_and(is_ident_char) {
                i += 1;
                let text: String = self.src[start..i].iter().collect();
                self.pos = i;
                return Ok(Token {
                    kind: Tok::Duration,
                    text,
                    pos: start,
                });
            }
            // A digit-led run that butts against identifier characters is an
            // identifier: `1-alpha`, `42day-sla`, `9.3.1`, `2026-07-14`. Re-lex
            // the whole thing from the start so version strings, dates, and
            // numeric-prefixed labels need no quotes on the value side.
            //
            // Signed forms are excluded on purpose: `-3-foo` is a typo, not a
            // label, and quietly turning it into an identifier would hide it.
            if !signed && is_ident_char(c) {
                return Ok(self.read_ident(start));
            }
        }

        let text: String = self.src[start..i].iter().collect();
        self.pos = i;
        Ok(Token {
            kind: Tok::Num,
            text,
            pos: start,
        })
    }

    fn read_ident(&mut self, start: usize) -> Token {
        let mut i = start;
        while self.at(i).is_some_and(is_ident_char) {
            i += 1;
        }
        let text: String = self.src[start..i].iter().collect();
        self.pos = i;

        let kind = match text.to_ascii_uppercase().as_str() {
            "AND" => Tok::And,
            "OR" => Tok::Or,
            "NOT" => Tok::Not,
            _ => Tok::Ident,
        };
        Token {
            kind,
            text,
            pos: start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Result<Vec<(Tok, String)>> {
        let mut lx = Lexer::new(input);
        let mut out = Vec::new();
        loop {
            let t = lx.next_token()?;
            if t.kind == Tok::Eof {
                return Ok(out);
            }
            out.push((t.kind, t.text));
        }
    }

    fn kinds(input: &str) -> Vec<Tok> {
        lex(input).unwrap().into_iter().map(|(k, _)| k).collect()
    }

    fn texts(input: &str) -> Vec<String> {
        lex(input).unwrap().into_iter().map(|(_, t)| t).collect()
    }

    #[test]
    fn operators_and_parens() {
        assert_eq!(
            kinds("(a=1 AND b!=2) OR c<3 OR d<=4 OR e>5 OR f>=6"),
            vec![
                Tok::LParen,
                Tok::Ident,
                Tok::Eq,
                Tok::Num,
                Tok::And,
                Tok::Ident,
                Tok::Ne,
                Tok::Num,
                Tok::RParen,
                Tok::Or,
                Tok::Ident,
                Tok::Lt,
                Tok::Num,
                Tok::Or,
                Tok::Ident,
                Tok::Le,
                Tok::Num,
                Tok::Or,
                Tok::Ident,
                Tok::Gt,
                Tok::Num,
                Tok::Or,
                Tok::Ident,
                Tok::Ge,
                Tok::Num,
            ]
        );
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            kinds("a=1 and b=2 or not c=3"),
            vec![
                Tok::Ident,
                Tok::Eq,
                Tok::Num,
                Tok::And,
                Tok::Ident,
                Tok::Eq,
                Tok::Num,
                Tok::Or,
                Tok::Not,
                Tok::Ident,
                Tok::Eq,
                Tok::Num,
            ]
        );
        // ...but a field or value that merely *contains* a keyword is not one.
        assert_eq!(kinds("android=1"), vec![Tok::Ident, Tok::Eq, Tok::Num]);
    }

    #[test]
    fn durations() {
        assert_eq!(kinds("updated>7d"), vec![Tok::Ident, Tok::Gt, Tok::Duration]);
        for d in ["7d", "24h", "2w", "3m", "1y", "7D", "24H"] {
            assert_eq!(kinds(&format!("a>{d}"))[2], Tok::Duration, "{d}");
        }
    }

    #[test]
    fn digit_led_tokens_relex_as_identifiers() {
        // The trap: each of these starts like a number and must not end as one.
        for v in ["1-alpha", "42day-sla", "9.3.1", "2026-07-14", "7days", "2b"] {
            let toks = lex(&format!("label={v}")).unwrap();
            assert_eq!(toks[2].0, Tok::Ident, "{v} should re-lex as an identifier");
            assert_eq!(toks[2].1, v);
        }
        // A bare number is still a number.
        assert_eq!(kinds("priority=1"), vec![Tok::Ident, Tok::Eq, Tok::Num]);
        // ...and so is a signed one.
        assert_eq!(texts("priority=-5"), vec!["priority", "=", "-5"]);
    }

    #[test]
    fn signed_digit_led_runs_do_not_relex() {
        // `-5-alpha` lexes as the number -5 followed by `-alpha`, which is a
        // sign with no digits: an error. The user has to quote it.
        let err = lex("label=-5-alpha").unwrap_err();
        assert_eq!(err, Error::UnexpectedChar { ch: '-', pos: 8 });
    }

    #[test]
    fn namespaced_labels_need_no_quotes() {
        assert_eq!(
            texts("label=gt:merge-request"),
            vec!["label", "=", "gt:merge-request"]
        );
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(texts(r#"title="hello world""#)[2], "hello world");
        assert_eq!(texts("title='hello world'")[2], "hello world");
        assert_eq!(texts(r#"title="a\nb\tc\\d\"e\'f""#)[2], "a\nb\tc\\d\"e'f");
        // An empty string is a legal value: `assignee=""` means unassigned.
        assert_eq!(texts(r#"assignee="""#)[2], "");
        // Quoting lets anything through, including operators and keywords.
        assert_eq!(texts(r#"title="AND (x>1)""#)[2], "AND (x>1)");
    }

    #[test]
    fn unterminated_string_is_an_error() {
        assert_eq!(
            lex(r#"title="oops"#).unwrap_err(),
            Error::UnterminatedString(6)
        );
        assert_eq!(lex("title='oops").unwrap_err(), Error::UnterminatedString(6));
        // A trailing backslash eats the closing quote.
        assert_eq!(
            lex(r#"title="oops\""#).unwrap_err(),
            Error::UnterminatedString(6)
        );
    }

    #[test]
    fn bare_bang_is_rejected() {
        assert_eq!(
            lex("!status=open").unwrap_err(),
            Error::UnexpectedChar { ch: '!', pos: 0 }
        );
    }

    #[test]
    fn positions_are_character_offsets() {
        let mut lx = Lexer::new("título=ok");
        assert_eq!(lx.next_token().unwrap().pos, 0);
        assert_eq!(lx.next_token().unwrap().pos, 6);
        assert_eq!(lx.next_token().unwrap().pos, 7);
    }
}

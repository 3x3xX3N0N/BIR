//! Binding variables, validating them, and substituting `{{name}}` in text.
//!
//! A formula declares variables; the caller supplies values; this module
//! reconciles the two into [`Bindings`] — a flat, validated map that later
//! stages read without ever having to think about defaults, requiredness, or
//! enums again. Every rule a variable can carry is enforced *here*, once, so
//! that by cook time a value is known-good.
//!
//! The rule that catches the most real bugs: **an unknown `{{var}}` is an
//! error, not an empty string.** A typo'd variable that silently expands to
//! nothing produces an issue titled "Implement " and no hint why. Substitution
//! fails loudly and names the variable instead.

use std::collections::BTreeMap;

use crate::types::{Formula, VarDef, VarType};
use crate::{Error, Result};

/// Resolved variables, ready to substitute. Also carries loop variables, which
/// are injected per-iteration during cooking and use single braces (`{i}`).
#[derive(Debug, Clone, Default)]
pub struct Bindings {
    /// Formula variables, referenced as `{{name}}`.
    vars: BTreeMap<String, String>,
    /// Loop variables, referenced as `{name}`. A separate namespace so a loop
    /// var named `n` cannot collide with a formula var named `n`.
    loop_vars: BTreeMap<String, String>,
}

impl Bindings {
    /// Reconcile declared variables with supplied ones.
    ///
    /// `provided` is what the caller passed (`--var k=v`). The result carries a
    /// value for every declared variable, drawn from `provided` or the default,
    /// and validated against the declaration. Order of operations matters:
    /// requiredness is checked *before* enum/pattern, so "you didn't give me x"
    /// beats "x is not one of a,b,c" when x is simply absent.
    pub fn bind(formula: &Formula, provided: &BTreeMap<String, String>) -> Result<Bindings> {
        let mut vars = BTreeMap::new();

        for (name, def) in &formula.vars {
            let value = match provided.get(name) {
                Some(v) => v.clone(),
                None => match &def.default {
                    Some(d) => d.clone(),
                    None if def.required => {
                        return Err(Error::Var(format!(
                            "required variable `{name}` was not provided"
                        )));
                    }
                    // No default, not required, not provided: the variable is
                    // simply unset. Referencing it in text is still an error
                    // (caught at substitution), but declaring it is fine.
                    None => continue,
                },
            };
            validate_value(name, def, &value)?;
            vars.insert(name.clone(), value);
        }

        // A value provided for a variable the formula never declared is almost
        // always a typo at the call site (`--var compnent=api`), and silently
        // ignoring it means the real variable keeps its default and nothing
        // says why. Refuse it and name it.
        for name in provided.keys() {
            if !formula.vars.contains_key(name) {
                return Err(Error::Var(format!(
                    "`{name}` was provided but this formula declares no such variable"
                )));
            }
        }

        Ok(Bindings {
            vars,
            loop_vars: BTreeMap::new(),
        })
    }

    /// A copy with one loop variable bound. Cooking a loop makes one of these
    /// per iteration, so the body sees `{i}` = the current value without the
    /// binding leaking to its siblings.
    pub fn with_loop_var(&self, name: &str, value: impl Into<String>) -> Bindings {
        let mut next = self.clone();
        next.loop_vars.insert(name.to_string(), value.into());
        next
    }

    /// Look up a formula variable.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Substitute `{{formula_var}}` and `{loop_var}` in `text`.
    ///
    /// Both syntaxes, one pass, left to right. `{{...}}` is checked first at each
    /// position so a formula var is never mistaken for two nested loop vars. An
    /// unresolved reference of either kind is an error naming the reference — a
    /// silent empty expansion is how a formula produces "Move  of " and no clue.
    pub fn substitute(&self, text: &str) -> Result<String> {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;

        while i < bytes.len() {
            if bytes[i] == b'{' {
                // `{{name}}` — a formula variable.
                if bytes.get(i + 1) == Some(&b'{') {
                    let (name, end) = read_until(text, i + 2, "}}")?;
                    let value = self.vars.get(name).ok_or_else(|| {
                        Error::Var(format!("`{{{{{name}}}}}` refers to an unset variable"))
                    })?;
                    out.push_str(value);
                    i = end;
                    continue;
                }
                // `{name}` — a loop variable. Only meaningful inside a loop body;
                // outside one, `loop_vars` is empty and this is an honest error.
                let (name, end) = read_until(text, i + 1, "}")?;
                let value = self.loop_vars.get(name).ok_or_else(|| {
                    Error::Var(format!(
                        "`{{{name}}}` refers to a loop variable that is not in scope here"
                    ))
                })?;
                out.push_str(value);
                i = end;
                continue;
            }
            // A plain byte. Copy it; pushing the char keeps multibyte UTF-8
            // intact because we only ever branch on ASCII `{`.
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
        Ok(out)
    }
}

/// Read the name between the current position and `close`, returning it and the
/// index just past the closing delimiter. An unterminated `{{` is a formula-text
/// bug and says so.
fn read_until<'a>(text: &'a str, from: usize, close: &str) -> Result<(&'a str, usize)> {
    match text[from..].find(close) {
        Some(rel) => {
            let name = text[from..from + rel].trim();
            Ok((name, from + rel + close.len()))
        }
        None => Err(Error::Var(format!(
            "unterminated `{}` in formula text: {text:?}",
            if close == "}}" { "{{" } else { "{" }
        ))),
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// Enforce a variable's declared constraints on a concrete value.
fn validate_value(name: &str, def: &VarDef, value: &str) -> Result<()> {
    if !def.enum_values.is_empty() && !def.enum_values.iter().any(|e| e == value) {
        return Err(Error::Var(format!(
            "`{name}` = {value:?} is not one of {:?}",
            def.enum_values
        )));
    }
    match def.var_type {
        VarType::Int => {
            if value.parse::<i64>().is_err() {
                return Err(Error::Var(format!(
                    "`{name}` is declared int but got {value:?}"
                )));
            }
        }
        VarType::Bool => {
            if !matches!(value, "true" | "false") {
                return Err(Error::Var(format!(
                    "`{name}` is declared bool but got {value:?} (want `true` or `false`)"
                )));
            }
        }
        VarType::String => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn provided(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn formula(src: &str) -> Formula {
        parse(src).unwrap()
    }

    #[test]
    fn a_missing_required_var_is_named() {
        let f = formula(
            r#"formula="f"
               version=1
               [vars.component]
               required = true
               [[steps]]
               id="a"
               title="t""#,
        );
        let err = Bindings::bind(&f, &provided(&[])).unwrap_err();
        assert!(err.to_string().contains("`component`"));
    }

    #[test]
    fn a_default_fills_in_for_an_absent_var() {
        let f = formula(
            r#"formula="f"
               version=1
               [vars.env]
               default = "staging"
               [[steps]]
               id="a"
               title="t""#,
        );
        let b = Bindings::bind(&f, &provided(&[])).unwrap();
        assert_eq!(b.get("env"), Some("staging"));
    }

    #[test]
    fn a_value_outside_the_enum_is_rejected() {
        let f = formula(
            r#"formula="f"
               version=1
               [vars.env]
               enum = ["staging", "prod"]
               [[steps]]
               id="a"
               title="t""#,
        );
        let err = Bindings::bind(&f, &provided(&[("env", "dev")])).unwrap_err();
        assert!(err.to_string().contains("not one of"));
    }

    #[test]
    fn an_undeclared_provided_var_is_a_typo_not_a_shrug() {
        let f = formula(
            r#"formula="f"
               version=1
               [vars.component]
               default = "x"
               [[steps]]
               id="a"
               title="t""#,
        );
        let err = Bindings::bind(&f, &provided(&[("compnent", "api")])).unwrap_err();
        assert!(err.to_string().contains("`compnent`"));
    }

    #[test]
    fn substitution_fills_a_formula_var() {
        let mut b = Bindings::default();
        b.vars.insert("name".into(), "auth".into());
        assert_eq!(b.substitute("Design {{name}}").unwrap(), "Design auth");
    }

    #[test]
    fn substitution_of_an_unset_var_is_a_loud_error() {
        let b = Bindings::default();
        let err = b.substitute("Design {{ghost}}").unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn a_loop_var_uses_single_braces_and_its_own_namespace() {
        let mut b = Bindings::default();
        b.vars.insert("n".into(), "FORMULA".into());
        let scoped = b.with_loop_var("n", "3");
        // `{n}` is the loop var, `{{n}}` is still the formula var — no collision.
        assert_eq!(scoped.substitute("Move {n}").unwrap(), "Move 3");
        assert_eq!(scoped.substitute("of {{n}}").unwrap(), "of FORMULA");
    }

    #[test]
    fn a_loop_var_out_of_scope_is_an_error_not_empty() {
        let b = Bindings::default();
        let err = b.substitute("Move {n}").unwrap_err();
        assert!(err.to_string().contains("not in scope"));
    }

    #[test]
    fn multibyte_text_survives_substitution() {
        let mut b = Bindings::default();
        b.vars.insert("who".into(), "wörld".into());
        assert_eq!(b.substitute("héllo {{who}} ✓").unwrap(), "héllo wörld ✓");
    }

    #[test]
    fn an_unterminated_reference_is_reported() {
        let b = Bindings::default();
        assert!(b.substitute("Design {{name").is_err());
    }
}

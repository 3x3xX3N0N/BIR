//! The formula AST — what a `.formula.toml` deserializes into.
//!
//! These types are the *parsed* shape, before any variable is bound or any loop
//! expanded. `serde` fills them from TOML; [`crate::cook`] consumes them. Field
//! names match upstream's on-disk format on purpose, so a formula written for Go
//! beads cooks here unchanged.

use serde::Deserialize;

/// A formula, as written.
#[derive(Debug, Clone, Deserialize)]
pub struct Formula {
    /// The unique name (`quick-check`, `feature-workflow`).
    pub formula: String,

    #[serde(default)]
    pub description: String,

    /// Schema version. Only `1` exists; anything else is refused at parse time
    /// rather than silently misread.
    #[serde(default = "one")]
    pub version: i64,

    #[serde(rename = "type", default)]
    pub kind: FormulaType,

    /// Parent formulas to inherit from. Parsed; cooking them is not built yet.
    #[serde(default)]
    pub extends: Vec<String>,

    /// Declared inputs. A `BTreeMap`, not a `HashMap`: the iteration order of
    /// vars is visible in error messages ("missing required: a, b, c"), and a
    /// diagnostic that reorders itself run to run is a worse diagnostic.
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, VarDef>,

    #[serde(default)]
    pub steps: Vec<Step>,

    /// Aspect-oriented step insertion. Parsed; not woven yet.
    #[serde(default)]
    pub advice: Vec<AdviceRule>,

    /// `bd pour` materializes each step as its own issue only when this is set;
    /// otherwise the steps are read inline. Carried through to the caller.
    #[serde(default)]
    pub pour: bool,
}

fn one() -> i64 {
    1
}

/// What a formula is for. Only [`Workflow`](FormulaType::Workflow) cooks today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormulaType {
    #[default]
    Workflow,
    /// A macro that expands into a target step. Parses; does not cook yet.
    Expansion,
    /// A cross-cutting concern applied to another formula. Parses; not yet.
    Aspect,
    /// A multi-agent parallel workflow. Parses; not yet.
    Convoy,
}

impl FormulaType {
    pub fn as_str(self) -> &'static str {
        match self {
            FormulaType::Workflow => "workflow",
            FormulaType::Expansion => "expansion",
            FormulaType::Aspect => "aspect",
            FormulaType::Convoy => "convoy",
        }
    }
}

/// A declared variable.
///
/// TOML lets this be written two ways, and both are in real formulas:
///
/// ```toml
/// [vars]
/// wisp_type = "patrol"        # a bare string IS the default
///
/// [vars.component]            # or a full table
/// description = "..."
/// required = true
/// ```
///
/// [`VarDef::deserialize`] handles both.
#[derive(Debug, Clone, Default)]
pub struct VarDef {
    pub description: String,
    /// `None` = no default. Distinct from `Some("")`: an explicit empty default
    /// is a valid, provided value; no default means the var must be supplied if
    /// anything references it.
    pub default: Option<String>,
    pub required: bool,
    /// Allowed values. Empty = unrestricted.
    pub enum_values: Vec<String>,
    /// `string` (default), `int`, or `bool`. Decides how `condition` compares it.
    pub var_type: VarType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VarType {
    #[default]
    String,
    Int,
    Bool,
}

impl VarType {
    fn parse(s: &str) -> Self {
        match s {
            "int" | "integer" => VarType::Int,
            "bool" | "boolean" => VarType::Bool,
            _ => VarType::String,
        }
    }
}

// A hand-written Deserialize, because `#[serde(untagged)]` would swallow the
// "which arm failed" detail that makes a bad var definition findable.
impl<'de> Deserialize<'de> for VarDef {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            Table {
                #[serde(default)]
                description: String,
                #[serde(default)]
                default: Option<String>,
                #[serde(default)]
                required: bool,
                #[serde(default, rename = "enum")]
                enum_values: Vec<String>,
                #[serde(default, rename = "type")]
                var_type: Option<String>,
            },
        }
        Ok(match Raw::deserialize(de)? {
            Raw::Bare(s) => VarDef {
                default: Some(s),
                ..Default::default()
            },
            Raw::Table {
                description,
                default,
                required,
                enum_values,
                var_type,
            } => VarDef {
                description,
                default,
                required,
                enum_values,
                var_type: var_type.as_deref().map(VarType::parse).unwrap_or_default(),
            },
        })
    }
}

/// One step of work. Becomes one issue — unless it loops (then N), or gates
/// (then two).
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    /// Unique within the formula. Used to reference the step from `needs`.
    pub id: String,

    /// The issue title. `{{var}}` and, inside a loop, `{loopvar}` substitute.
    #[serde(default)]
    pub title: String,

    #[serde(default)]
    pub description: String,

    #[serde(default)]
    pub notes: String,

    /// Issue type: `task` (default), `bug`, `feature`, `epic`, `chore`, …. An
    /// unknown value is a custom type, not an error — matching the store.
    #[serde(rename = "type", default)]
    pub issue_type: Option<String>,

    /// 0 (most urgent) .. 4. `None` means "let the store default it".
    #[serde(default)]
    pub priority: Option<i32>,

    #[serde(default)]
    pub labels: Vec<String>,

    /// Carried onto the created issue verbatim.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,

    /// Step ids this one blocks on. `needs` and `depends_on` are the same thing
    /// spelled two ways; both appear in real formulas, and [`Step::blockers`]
    /// merges them.
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Include this step only if the condition holds. `None` = always.
    #[serde(default)]
    pub condition: Option<String>,

    /// Expand this step over a range/count. `None` = a single step.
    #[serde(default, rename = "loop")]
    pub loop_spec: Option<LoopSpec>,

    /// Turn this step into a wait. `None` = ordinary work.
    #[serde(default)]
    pub gate: Option<GateSpec>,
}

impl Step {
    /// The step ids this step blocks on — `needs` and `depends_on`, deduped,
    /// order-preserving.
    pub fn blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        for id in self.needs.iter().chain(self.depends_on.iter()) {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }
}

/// A loop over a step body.
///
/// `range = "1..3"` binds `var` to 1, 2, 3 in turn; `count = 3` binds it to
/// 1, 2, 3 as well but reads better when the bound is not a range. Exactly one
/// of the two must be present.
#[derive(Debug, Clone, Deserialize)]
pub struct LoopSpec {
    #[serde(default)]
    pub range: Option<String>,
    #[serde(default)]
    pub count: Option<i64>,
    /// The iteration variable, exposed to the body as `{var}` (single braces,
    /// unlike formula vars). Defaults to `i`.
    #[serde(default = "loop_var_default")]
    pub var: String,
    /// The step(s) repeated. Their ids are suffixed with the iteration number so
    /// they stay unique across copies.
    #[serde(default)]
    pub body: Vec<Step>,
}

fn loop_var_default() -> String {
    "i".to_string()
}

/// An async wait attached to a step.
#[derive(Debug, Clone, Deserialize)]
pub struct GateSpec {
    /// What is being waited on: `timer`, `review`, `ci`, … Opaque here; the
    /// caller and downstream tooling interpret it.
    #[serde(rename = "type", default)]
    pub await_type: String,
    /// A stable id for the wait, so a resumed cook can find it again.
    #[serde(default)]
    pub await_id: Option<String>,
    /// How long before the wait gives up (`30m`, `2h`). Opaque duration string.
    #[serde(default)]
    pub timeout: Option<String>,
}

/// Aspect-oriented advice. Parsed so a formula that carries it round-trips, but
/// weaving it is not built — see [`crate::cook`].
#[derive(Debug, Clone, Deserialize)]
pub struct AdviceRule {
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub when: String,
    #[serde(default)]
    pub steps: Vec<Step>,
}

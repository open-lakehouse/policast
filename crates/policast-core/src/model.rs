use serde::{Deserialize, Serialize};

/// The effect a Cedar policy has when matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Permit,
    Forbid,
}

/// The kind of governance enforcement a compiled policy represents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterType {
    RowFilter,
    ColumnMask,
    DenyOverride,
}

/// Which principals a policy applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliesTo {
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub principals: Vec<String>,
}

/// A single compiled policy rule: the Cedar source has been parsed and its
/// condition expressions translated into a portable CEL string.
///
/// A policy targets either a concrete table (`target_table`) or a tag
/// expression (`target_tag`). Similarly, a column mask targets either a
/// concrete column (`column`) or a column-tag expression
/// (`applies_to_tag`). Tag-scoped policies are expanded into concrete
/// `(table, column)` bindings by the resolver before engines see them;
/// the engines themselves never interpret tag fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledPolicy {
    pub id: String,
    pub effect: Effect,
    pub filter_type: FilterType,
    /// Fully-qualified table name, `a.b.*` schema wildcard, or `*` for
    /// any table. May be `*` when `target_tag` carries the real scope.
    pub target_table: String,
    /// Concrete column name for a column_mask policy, or `None` when
    /// `applies_to_tag` carries the column selector (or the policy is a
    /// row_filter / deny_override).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// Tag expression that selects which tables the policy applies to.
    /// For v1 this is a bare tag name (`"pii"`); future versions may
    /// accept boolean expressions. When set, the resolver expands the
    /// policy into one concrete [`CompiledPolicy`] per matching table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_tag: Option<String>,
    /// Tag expression that selects which columns a column_mask applies
    /// to. Mutually exclusive with [`CompiledPolicy::column`]. Expanded
    /// by the resolver identically to `target_tag`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applies_to_tag: Option<String>,
    pub cel_expression: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applies_to: Option<AppliesTo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl CompiledPolicy {
    /// Returns true when this policy needs resolver-side expansion
    /// (i.e. either `target_tag` or `applies_to_tag` is set).
    pub fn is_tag_scoped(&self) -> bool {
        self.target_tag.is_some() || self.applies_to_tag.is_some()
    }
}

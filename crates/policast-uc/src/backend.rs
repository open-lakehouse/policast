//! Pluggable read backends for the resolver.
//!
//! The resolver needs to read four things to answer a request: the
//! compiled manifest (CEL expressions), the per-policy metadata from
//! the policies table, the bindings that connect principals to
//! policies for a given table, and — for tag-scoped templates — the
//! tag index that maps tables and columns to tags. Those rows can
//! live either in Delta tables (UC-OSS production deployment) or in
//! flat JSON files (unit tests and `examples/run_datafusion_uc.rs`).
//!
//! Both backends produce the same row types defined below, so the
//! actual resolve logic in [`crate::store::ResolverCore`] does not
//! care which it is talking to.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::UcError;

/// One row from `governance.policast.policies`.
///
/// Legacy, non-tag-scoped policies leave `target_tag` and
/// `applies_to_tag` unset; the resolver passes them through to the
/// [`policast_core::model::CompiledPolicy`] it emits, and
/// [`crate::store::ResolverCore`] decides at resolve-time whether to
/// fan a template out over the tag index. The deserialization uses
/// `#[serde(default)]` so existing `policies.json` rows (and
/// `policies` Delta rows written before the templates feature
/// landed) keep parsing verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyRow {
    pub policy_id: String,
    pub filter_type: String,
    pub target_table: String,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub target_tag: Option<String>,
    #[serde(default)]
    pub applies_to_tag: Option<String>,
    pub effect: String,
    #[serde(default)]
    pub applies_to_roles: Option<Vec<String>>,
    #[serde(default)]
    pub description: Option<String>,
    pub version: i64,
}

/// One row from `governance.policast.manifest`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestRow {
    pub policy_id: String,
    pub cel_expression: String,
    pub version: i64,
    #[serde(default)]
    pub compiler_version: String,
    #[serde(default)]
    pub source_hash: String,
}

/// One row from `governance.policast.bindings`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindingRow {
    pub binding_id: String,
    pub policy_id: String,
    pub target: String,
    /// `role:<name>`, `group:<name>`, `principal:<id>`, or `*`.
    pub principal_selector: String,
    #[serde(default)]
    pub precedence: i32,
}

/// One row from `governance.policast.tags`.
///
/// See `examples/uc/ddl/06_tags.sql` for the canonical schema. The
/// `entity` / `entity_kind` pair identifies *what* is being tagged:
///
/// * `entity_kind = "table"`  — `entity` is a fully-qualified table
///   name such as `hospital.clinical.patients`.
/// * `entity_kind = "column"` — `entity` is `<table>:<column>`, e.g.
///   `hospital.clinical.patients:ssn`.
///
/// Retired rows (`retired_at is not None`) are carried through so
/// callers can render history; [`crate::store::ResolverCore`] filters
/// them out during tag expansion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagRow {
    pub entity: String,
    pub entity_kind: String,
    pub tag: String,
    #[serde(default)]
    pub set_by: Option<String>,
    #[serde(default)]
    pub set_at: Option<String>,
    #[serde(default)]
    pub retired_at: Option<String>,
}

impl TagRow {
    /// Returns true if this row is still active (not tombstoned).
    pub fn is_active(&self) -> bool {
        self.retired_at.is_none()
    }

    /// Returns true if the row tags a table.
    pub fn is_table(&self) -> bool {
        self.entity_kind == "table"
    }

    /// Returns true if the row tags a column.
    pub fn is_column(&self) -> bool {
        self.entity_kind == "column"
    }

    /// Split a column entity into `(table, column)`; returns `None`
    /// for non-column rows or malformed entities.
    pub fn as_table_column(&self) -> Option<(&str, &str)> {
        if !self.is_column() {
            return None;
        }
        let (table, column) = self.entity.rsplit_once(':')?;
        if table.is_empty() || column.is_empty() {
            return None;
        }
        Some((table, column))
    }
}

/// Source of governance state for the resolver.
#[async_trait]
pub trait ResolveBackend: Send + Sync {
    async fn policies(&self) -> Result<Vec<PolicyRow>, UcError>;
    async fn manifest(&self) -> Result<Vec<ManifestRow>, UcError>;
    async fn bindings(&self) -> Result<Vec<BindingRow>, UcError>;

    /// Tag assignments used for resolver-side expansion of tag-scoped
    /// policy templates. Backends that do not ship a tag index (legacy
    /// deployments, minimal tests) can fall through to the default
    /// implementation, which returns an empty slice — the resolver
    /// treats that as "no templates will expand" rather than as an
    /// error.
    async fn tags(&self) -> Result<Vec<TagRow>, UcError> {
        Ok(Vec::new())
    }
}

/// Flat-file backend for the resolver. Reads three JSON files from a
/// "store root" directory:
///
/// ```text
///   policies.json  { "rows": [ PolicyRow, ... ] }
///   manifest.json  { "rows": [ ManifestRow, ... ] }
///   bindings.json  { "rows": [ BindingRow, ... ] }
/// ```
///
/// Use this for local development, unit tests, and the bundled
/// `examples/run_datafusion_uc.rs` demo.
#[derive(Debug, Clone)]
pub struct FileBackend {
    root: PathBuf,
}

impl FileBackend {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn read<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Result<Vec<T>, UcError> {
        let path = self.root.join(name);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            UcError::Io(std::io::Error::new(
                e.kind(),
                format!("reading {}: {e}", path.display()),
            ))
        })?;
        #[derive(Deserialize)]
        struct Wrapper<T> {
            rows: Vec<T>,
        }
        let wrapped: Wrapper<T> = serde_json::from_str(&text)?;
        Ok(wrapped.rows)
    }

    /// Read an optional file: a missing file yields an empty `Vec`
    /// rather than an error. Used for the tag index so older flat
    /// stores keep resolving without a `tags.json`.
    fn read_optional<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Result<Vec<T>, UcError> {
        let path = self.root.join(name);
        if !path.exists() {
            return Ok(Vec::new());
        }
        self.read(name)
    }
}

#[async_trait]
impl ResolveBackend for FileBackend {
    async fn policies(&self) -> Result<Vec<PolicyRow>, UcError> {
        self.read("policies.json")
    }
    async fn manifest(&self) -> Result<Vec<ManifestRow>, UcError> {
        self.read("manifest.json")
    }
    async fn bindings(&self) -> Result<Vec<BindingRow>, UcError> {
        self.read("bindings.json")
    }
    async fn tags(&self) -> Result<Vec<TagRow>, UcError> {
        self.read_optional("tags.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[tokio::test]
    async fn test_file_backend_reads_all_three() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "policies.json",
            r#"{"rows":[{"policy_id":"p1","filter_type":"row_filter","target_table":"t","effect":"permit","version":1}]}"#,
        );
        write(
            dir.path(),
            "manifest.json",
            r#"{"rows":[{"policy_id":"p1","cel_expression":"true","version":1}]}"#,
        );
        write(
            dir.path(),
            "bindings.json",
            r#"{"rows":[{"binding_id":"b1","policy_id":"p1","target":"t","principal_selector":"*","precedence":0}]}"#,
        );
        let b = FileBackend::new(dir.path());
        assert_eq!(b.policies().await.unwrap().len(), 1);
        assert_eq!(b.manifest().await.unwrap().len(), 1);
        assert_eq!(b.bindings().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_file_backend_missing_file_errors() {
        let dir = tempdir().unwrap();
        let b = FileBackend::new(dir.path());
        assert!(b.policies().await.is_err());
    }

    #[tokio::test]
    async fn test_file_backend_malformed_json_errors() {
        let dir = tempdir().unwrap();
        write(dir.path(), "policies.json", "not json");
        write(dir.path(), "manifest.json", r#"{"rows":[]}"#);
        write(dir.path(), "bindings.json", r#"{"rows":[]}"#);
        let b = FileBackend::new(dir.path());
        assert!(b.policies().await.is_err());
    }

    /// Reading an absent `tags.json` returns an empty list rather than
    /// an error. Legacy flat stores predate tag support and must keep
    /// resolving without surgery.
    #[tokio::test]
    async fn test_file_backend_missing_tags_is_empty() {
        let dir = tempdir().unwrap();
        let b = FileBackend::new(dir.path());
        let tags = b.tags().await.unwrap();
        assert!(tags.is_empty());
    }

    /// Populated `tags.json` round-trips through the FileBackend.
    #[tokio::test]
    async fn test_file_backend_reads_tags() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "tags.json",
            r#"{
                "rows": [
                    {"entity":"hospital.clinical.patients","entity_kind":"table","tag":"clinical","set_by":"admin","set_at":"2026-01-01T00:00:00Z"},
                    {"entity":"hospital.clinical.patients:ssn","entity_kind":"column","tag":"pii","set_by":"admin","set_at":"2026-01-01T00:00:00Z"}
                ]
            }"#,
        );
        let b = FileBackend::new(dir.path());
        let tags = b.tags().await.unwrap();
        assert_eq!(tags.len(), 2);
        assert!(tags.iter().any(|t| t.tag == "clinical" && t.is_table()));
        assert!(tags.iter().any(|t| t.tag == "pii" && t.is_column()));
    }

    /// Malformed `tags.json` surfaces an error — silent drops would
    /// let a corrupt tag index disable governance.
    #[tokio::test]
    async fn test_file_backend_malformed_tags_errors() {
        let dir = tempdir().unwrap();
        write(dir.path(), "tags.json", "not json");
        let b = FileBackend::new(dir.path());
        assert!(b.tags().await.is_err());
    }

    #[test]
    fn test_tag_row_active_and_kind_helpers() {
        let table_tag = TagRow {
            entity: "cat.sch.tab".into(),
            entity_kind: "table".into(),
            tag: "clinical".into(),
            set_by: None,
            set_at: None,
            retired_at: None,
        };
        assert!(table_tag.is_table());
        assert!(!table_tag.is_column());
        assert!(table_tag.is_active());
        assert_eq!(table_tag.as_table_column(), None);

        let retired = TagRow {
            retired_at: Some("2026-02-02T00:00:00Z".into()),
            ..table_tag.clone()
        };
        assert!(!retired.is_active());
    }

    #[test]
    fn test_tag_row_as_table_column_splits_correctly() {
        let col = TagRow {
            entity: "hospital.clinical.patients:ssn".into(),
            entity_kind: "column".into(),
            tag: "pii".into(),
            set_by: None,
            set_at: None,
            retired_at: None,
        };
        assert_eq!(
            col.as_table_column(),
            Some(("hospital.clinical.patients", "ssn"))
        );

        let bogus_missing_column = TagRow {
            entity: "hospital.clinical.patients".into(),
            entity_kind: "column".into(),
            ..col.clone()
        };
        assert_eq!(bogus_missing_column.as_table_column(), None);

        let bogus_empty_table = TagRow {
            entity: ":ssn".into(),
            entity_kind: "column".into(),
            ..col.clone()
        };
        assert_eq!(bogus_empty_table.as_table_column(), None);

        let bogus_empty_column = TagRow {
            entity: "hospital.clinical.patients:".into(),
            entity_kind: "column".into(),
            ..col.clone()
        };
        assert_eq!(bogus_empty_column.as_table_column(), None);
    }

    /// Default implementation of `tags()` on a hand-rolled backend
    /// that only implements the required three methods should return
    /// an empty list — this is the contract that keeps older backends
    /// compiling when the trait grows.
    #[tokio::test]
    async fn test_default_tags_impl_returns_empty() {
        struct MinimalBackend;
        #[async_trait]
        impl ResolveBackend for MinimalBackend {
            async fn policies(&self) -> Result<Vec<PolicyRow>, UcError> {
                Ok(Vec::new())
            }
            async fn manifest(&self) -> Result<Vec<ManifestRow>, UcError> {
                Ok(Vec::new())
            }
            async fn bindings(&self) -> Result<Vec<BindingRow>, UcError> {
                Ok(Vec::new())
            }
        }

        let b = MinimalBackend;
        let tags = b.tags().await.unwrap();
        assert!(tags.is_empty());
    }

    /// Bundled `examples/uc/store/tags.json` round-trips cleanly — a
    /// regression guard for the shipped demo seed.
    #[tokio::test]
    async fn test_shipped_tags_json_parses() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/uc/store");
        let b = FileBackend::new(root);
        let tags = b.tags().await.unwrap();
        assert!(
            tags.iter()
                .any(|t| t.tag == "pii" && t.entity.ends_with(":ssn")),
            "expected pii tag on ssn column, got: {tags:?}"
        );
        assert!(
            tags.iter()
                .any(|t| t.tag == "phi" && t.entity.ends_with(":diagnosis")),
            "expected phi tag on diagnosis column, got: {tags:?}"
        );
        assert!(
            tags.iter()
                .any(|t| t.tag == "clinical" && t.is_table()),
            "expected clinical tag on the patients table, got: {tags:?}"
        );
    }
}

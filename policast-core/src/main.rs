use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use policast_core::model::{CompiledPolicy, Effect, FilterType};
use policast_core::{parse_policies, PolicyManifest};

#[derive(Parser, Debug)]
#[command(
    name = "policast",
    about = "Compile Cedar policies into CEL expressions and publish to a UC-style policy store"
)]
struct Cli {
    /// Legacy positional Cedar files (compiled and written to --output).
    /// Equivalent to `policast compile <files>` when no subcommand is given.
    #[arg(global = false)]
    files: Vec<PathBuf>,

    /// Output file for the policy manifest JSON (stdout if omitted).
    /// Only used when `files` are provided without a subcommand.
    #[arg(short, long)]
    output: Option<PathBuf>,

    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Compile Cedar sources to a PolicyManifest JSON file (default behavior).
    Compile {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Unity Catalog policy-store operations.
    Uc {
        #[command(subcommand)]
        op: UcOp,
    },
}

#[derive(Subcommand, Debug)]
enum UcOp {
    /// Compile Cedar sources and MERGE them into a UC-shaped policy
    /// store (policies.json + manifest.json under `--store-root`).
    Publish {
        /// Directory containing policies.json / manifest.json / bindings.json.
        #[arg(long)]
        store_root: PathBuf,
        /// Cedar source files to compile and publish.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },

    /// Add a (policy, target, principal) binding to bindings.json and
    /// update the target table's property overlay if one exists.
    Bind {
        #[arg(long)]
        store_root: PathBuf,
        #[arg(long)]
        policy: String,
        #[arg(long)]
        target: String,
        #[arg(long = "principal-selector")]
        principal_selector: String,
        #[arg(long, default_value_t = 0)]
        precedence: i32,
        /// Optional properties overlay JSON file (e.g. patients.properties.json)
        /// that will be refreshed with the new `policast.applied_policies`.
        #[arg(long)]
        properties_file: Option<PathBuf>,
    },

    /// Diff two manifest.json files (typically the live store vs a
    /// snapshot from Delta time travel).
    Diff {
        #[arg(long)]
        before: PathBuf,
        #[arg(long)]
        after: PathBuf,
    },
}

// --- Flat-file store rowtypes, mirrored from policast-uc::backend ---
// Mirrored (not imported) so policast-core has no optional dep on
// policast-uc; the wire shape is frozen by the design doc.

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Rows<T> {
    rows: Vec<T>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PolicyRow {
    policy_id: String,
    filter_type: String,
    target_table: String,
    #[serde(default)]
    column: Option<String>,
    effect: String,
    #[serde(default)]
    applies_to_roles: Option<Vec<String>>,
    #[serde(default)]
    description: Option<String>,
    version: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ManifestRow {
    policy_id: String,
    cel_expression: String,
    version: i64,
    #[serde(default)]
    compiler_version: String,
    #[serde(default)]
    source_hash: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BindingRow {
    binding_id: String,
    policy_id: String,
    target: String,
    principal_selector: String,
    #[serde(default)]
    precedence: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PropertiesOverlay {
    table: String,
    #[serde(default)]
    properties: BTreeMap<String, String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match (&cli.command, cli.files.is_empty()) {
        (Some(Commands::Compile { files, output }), _) => {
            compile_to_file(files, output.as_deref(), cli.verbose)
        }
        (Some(Commands::Uc { op }), _) => run_uc(op, cli.verbose),
        (None, false) => compile_to_file(&cli.files, cli.output.as_deref(), cli.verbose),
        (None, true) => {
            eprintln!("error: provide Cedar files or a subcommand; see --help");
            std::process::exit(2);
        }
    }
}

fn compile_to_file(
    files: &[PathBuf],
    output: Option<&Path>,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = compile_all(files, verbose)?;
    let json = manifest.to_json()?;
    if let Some(out) = output {
        std::fs::write(out, &json)?;
        eprintln!("Wrote manifest to {}", out.display());
    } else {
        println!("{json}");
    }
    Ok(())
}

fn compile_all(files: &[PathBuf], verbose: bool) -> Result<PolicyManifest, Box<dyn std::error::Error>> {
    let mut manifest = PolicyManifest::new();
    for path in files {
        let cedar_text = std::fs::read_to_string(path)?;
        if verbose {
            eprintln!("Parsing: {}", path.display());
        }
        let parsed = parse_policies(&cedar_text)?;
        if verbose {
            eprintln!("  Found {} policies", parsed.len());
        }
        manifest.compile_policies(&parsed)?;
    }
    Ok(manifest)
}

fn run_uc(op: &UcOp, verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    match op {
        UcOp::Publish { store_root, files } => uc_publish(store_root, files, verbose),
        UcOp::Bind {
            store_root,
            policy,
            target,
            principal_selector,
            precedence,
            properties_file,
        } => uc_bind(
            store_root,
            policy,
            target,
            principal_selector,
            *precedence,
            properties_file.as_deref(),
        ),
        UcOp::Diff { before, after } => uc_diff(before, after),
    }
}

fn uc_publish(
    store_root: &Path,
    files: &[PathBuf],
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = compile_all(files, verbose)?;
    std::fs::create_dir_all(store_root)?;

    let mut policies: Vec<PolicyRow> = load_rows(store_root, "policies.json").unwrap_or_default();
    let mut manifest_rows: Vec<ManifestRow> =
        load_rows(store_root, "manifest.json").unwrap_or_default();

    let compiler_version = env!("CARGO_PKG_VERSION").to_string();

    for compiled in &manifest.policies {
        let next_version = policies
            .iter()
            .filter(|p| p.policy_id == compiled.id)
            .map(|p| p.version)
            .max()
            .unwrap_or(0)
            + 1;
        let row = compiled_to_row(compiled, next_version);
        upsert_policy(&mut policies, row);
        upsert_manifest(
            &mut manifest_rows,
            ManifestRow {
                policy_id: compiled.id.clone(),
                cel_expression: compiled.cel_expression.clone(),
                version: next_version,
                compiler_version: compiler_version.clone(),
                source_hash: format!("sha256:{}@v{}", compiled.id, next_version),
            },
        );
    }

    save_rows(store_root, "policies.json", &policies)?;
    save_rows(store_root, "manifest.json", &manifest_rows)?;

    eprintln!(
        "policast uc publish: {} policies merged into {}",
        manifest.policies.len(),
        store_root.display()
    );
    Ok(())
}

fn uc_bind(
    store_root: &Path,
    policy: &str,
    target: &str,
    selector: &str,
    precedence: i32,
    properties_file: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bindings: Vec<BindingRow> =
        load_rows(store_root, "bindings.json").unwrap_or_default();
    // Upsert by (policy, target, selector) — users can re-run `bind`
    // idempotently to bump precedence.
    bindings.retain(|b| {
        !(b.policy_id == policy && b.target == target && b.principal_selector == selector)
    });
    let binding_id = format!(
        "b-{}-{}-{}",
        short_hash(policy),
        short_hash(target),
        short_hash(selector)
    );
    bindings.push(BindingRow {
        binding_id,
        policy_id: policy.to_string(),
        target: target.to_string(),
        principal_selector: selector.to_string(),
        precedence,
    });
    save_rows(store_root, "bindings.json", &bindings)?;

    if let Some(props_path) = properties_file {
        refresh_properties_overlay(props_path, target, &bindings)?;
    }

    eprintln!(
        "policast uc bind: {} -> {} for {} (precedence {})",
        policy, target, selector, precedence
    );
    Ok(())
}

fn uc_diff(before: &Path, after: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let b_text = std::fs::read_to_string(before)?;
    let a_text = std::fs::read_to_string(after)?;
    let b_rows: Rows<ManifestRow> = serde_json::from_str(&b_text)?;
    let a_rows: Rows<ManifestRow> = serde_json::from_str(&a_text)?;
    let by_id = |rows: &[ManifestRow]| -> BTreeMap<String, ManifestRow> {
        rows.iter().map(|r| (r.policy_id.clone(), r.clone())).collect()
    };
    let b = by_id(&b_rows.rows);
    let a = by_id(&a_rows.rows);

    let mut changed = 0;
    for (id, after_row) in &a {
        match b.get(id) {
            None => {
                println!("+ {id}: (new) cel = {}", after_row.cel_expression);
                changed += 1;
            }
            Some(before_row) if before_row.cel_expression != after_row.cel_expression => {
                println!(
                    "~ {id}:\n    before: {}\n    after:  {}",
                    before_row.cel_expression, after_row.cel_expression
                );
                changed += 1;
            }
            Some(_) => {}
        }
    }
    for (id, before_row) in &b {
        if !a.contains_key(id) {
            println!("- {id}: (removed) cel was = {}", before_row.cel_expression);
            changed += 1;
        }
    }
    if changed == 0 {
        eprintln!("policast uc diff: no changes");
    } else {
        eprintln!("policast uc diff: {changed} change(s)");
    }
    Ok(())
}

fn load_rows<T>(root: &Path, name: &str) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: for<'de> Deserialize<'de>,
{
    let path = root.join(name);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    let wrapped: Rows<T> = serde_json::from_str(&text)?;
    Ok(wrapped.rows)
}

fn save_rows<T: Serialize>(
    root: &Path,
    name: &str,
    rows: &[T],
) -> Result<(), Box<dyn std::error::Error>> {
    #[derive(Serialize)]
    struct Wrap<'a, T> {
        rows: &'a [T],
    }
    let w = Wrap { rows };
    let text = serde_json::to_string_pretty(&w)?;
    std::fs::write(root.join(name), text + "\n")?;
    Ok(())
}

fn compiled_to_row(p: &CompiledPolicy, version: i64) -> PolicyRow {
    PolicyRow {
        policy_id: p.id.clone(),
        filter_type: match p.filter_type {
            FilterType::RowFilter => "row_filter",
            FilterType::ColumnMask => "column_mask",
            FilterType::DenyOverride => "deny_override",
        }
        .to_string(),
        target_table: p.target_table.clone(),
        column: p.column.clone(),
        effect: match p.effect {
            Effect::Permit => "permit",
            Effect::Forbid => "forbid",
        }
        .to_string(),
        applies_to_roles: p.applies_to.as_ref().map(|a| a.roles.clone()),
        description: p.description.clone(),
        version,
    }
}

fn upsert_policy(rows: &mut Vec<PolicyRow>, row: PolicyRow) {
    rows.retain(|r| !(r.policy_id == row.policy_id && r.version == row.version));
    rows.push(row);
}

fn upsert_manifest(rows: &mut Vec<ManifestRow>, row: ManifestRow) {
    rows.retain(|r| !(r.policy_id == row.policy_id && r.version == row.version));
    rows.push(row);
}

fn refresh_properties_overlay(
    path: &Path,
    target: &str,
    bindings: &[BindingRow],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut overlay: PropertiesOverlay = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(path)?)?
    } else {
        PropertiesOverlay {
            table: target.to_string(),
            properties: BTreeMap::new(),
        }
    };
    let applied: Vec<&str> = bindings
        .iter()
        .filter(|b| b.target == target)
        .map(|b| b.policy_id.as_str())
        .collect();
    overlay
        .properties
        .insert("policast.applied_policies".into(), applied.join(","));
    std::fs::write(path, serde_json::to_string_pretty(&overlay)? + "\n")?;
    Ok(())
}

fn short_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use policast_core::model::AppliesTo;

    fn sample_policy(id: &str) -> CompiledPolicy {
        CompiledPolicy {
            id: id.into(),
            effect: Effect::Permit,
            filter_type: FilterType::RowFilter,
            target_table: "patients".into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: "true".into(),
            applies_to: Some(AppliesTo {
                roles: vec!["analyst".into()],
                principals: Vec::new(),
            }),
            description: Some("d".into()),
        }
    }

    #[test]
    fn test_compiled_to_row_preserves_fields() {
        let p = sample_policy("p1");
        let row = compiled_to_row(&p, 7);
        assert_eq!(row.policy_id, "p1");
        assert_eq!(row.filter_type, "row_filter");
        assert_eq!(row.effect, "permit");
        assert_eq!(row.target_table, "patients");
        assert_eq!(row.version, 7);
        assert_eq!(row.applies_to_roles.as_deref(), Some(&["analyst".to_string()][..]));
    }

    #[test]
    fn test_upsert_policy_replaces_by_id_and_version() {
        let mut rows = vec![compiled_to_row(&sample_policy("p1"), 1)];
        let mut updated = sample_policy("p1");
        updated.cel_expression = "changed".into();
        upsert_policy(&mut rows, compiled_to_row(&updated, 1));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_upsert_policy_keeps_different_versions() {
        let mut rows = vec![compiled_to_row(&sample_policy("p1"), 1)];
        upsert_policy(&mut rows, compiled_to_row(&sample_policy("p1"), 2));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_short_hash_is_deterministic() {
        assert_eq!(short_hash("role:analyst"), short_hash("role:analyst"));
        assert_ne!(short_hash("role:analyst"), short_hash("role:physician"));
        assert_eq!(short_hash("x").len(), 8);
    }

    #[test]
    fn test_uc_diff_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let before = dir.path().join("before.json");
        let after = dir.path().join("after.json");

        let r1 = Rows {
            rows: vec![ManifestRow {
                policy_id: "p1".into(),
                cel_expression: "a".into(),
                version: 1,
                compiler_version: "0.0.0".into(),
                source_hash: String::new(),
            }],
        };
        let r2 = Rows {
            rows: vec![
                ManifestRow {
                    policy_id: "p1".into(),
                    cel_expression: "b".into(),
                    version: 2,
                    compiler_version: "0.0.0".into(),
                    source_hash: String::new(),
                },
                ManifestRow {
                    policy_id: "p2".into(),
                    cel_expression: "new".into(),
                    version: 1,
                    compiler_version: "0.0.0".into(),
                    source_hash: String::new(),
                },
            ],
        };
        std::fs::write(&before, serde_json::to_string(&r1).unwrap()).unwrap();
        std::fs::write(&after, serde_json::to_string(&r2).unwrap()).unwrap();

        uc_diff(&before, &after).unwrap();
    }

    #[test]
    fn test_refresh_properties_overlay_creates_and_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("props.json");
        let bindings = vec![
            BindingRow {
                binding_id: "b1".into(),
                policy_id: "p1".into(),
                target: "t".into(),
                principal_selector: "*".into(),
                precedence: 0,
            },
            BindingRow {
                binding_id: "b2".into(),
                policy_id: "p2".into(),
                target: "t".into(),
                principal_selector: "*".into(),
                precedence: 0,
            },
            BindingRow {
                binding_id: "b3".into(),
                policy_id: "p3".into(),
                target: "other".into(),
                principal_selector: "*".into(),
                precedence: 0,
            },
        ];
        refresh_properties_overlay(&path, "t", &bindings).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let overlay: PropertiesOverlay = serde_json::from_str(&text).unwrap();
        assert_eq!(overlay.properties["policast.applied_policies"], "p1,p2");
    }
}

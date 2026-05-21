-- Canonical Cedar policy registry. One row per (policy_id, version).
-- Mirrors policast_core::model::CompiledPolicy 1:1 so JSON round-trip
-- with PolicyManifest is trivial.

CREATE TABLE IF NOT EXISTS governance.policast.policies (
    policy_id          STRING       NOT NULL,
    cedar_source       STRING       NOT NULL,
    filter_type        STRING       NOT NULL  COMMENT 'row_filter | column_mask | deny_override',
    target_table       STRING       NOT NULL  COMMENT 'catalog.schema.table or wildcard',
    column             STRING                 COMMENT 'set for column_mask policies only (concrete, non-template policies)',
    target_tag         STRING                 COMMENT 'template: expand to every table carrying this tag (mutually exclusive with a concrete target_table pointer — use "*" or a namespace prefix there)',
    applies_to_tag     STRING                 COMMENT 'template: expand to every column carrying this tag; paired with a column_mask filter_type',
    effect             STRING       NOT NULL  COMMENT 'permit | forbid',
    applies_to_roles   ARRAY<STRING>          COMMENT 'from @roles(...) annotation',
    description        STRING,
    version            BIGINT       NOT NULL,
    created_by         STRING       NOT NULL,
    created_at         TIMESTAMP    NOT NULL,
    retired_at         TIMESTAMP              COMMENT 'tombstone for retired versions'
)
USING DELTA
PARTITIONED BY (policy_id)
COMMENT 'Cedar-authored policies, versioned';

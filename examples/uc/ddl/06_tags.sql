-- Tag index. One row per (entity, tag) assignment. The resolver uses
-- this table to expand tag-scoped Cedar templates (policies carrying
-- `@target_tag(...)` or `@applies_to_tag(...)`) into concrete
-- (table, column) bindings before signing the resolve bundle.
--
-- Entity grammar
--   entity_kind = 'table'  -> entity is a fully-qualified table name,
--                             e.g. 'hospital.clinical.patients'
--   entity_kind = 'column' -> entity is '<table>:<column>', e.g.
--                             'hospital.clinical.patients:ssn'
--
-- A single entity may carry multiple tags; a single tag may be attached
-- to many entities. The resolver treats tags as an unordered set.
--
-- Change Data Feed is enabled so sidecars / engines can tail an
-- invalidation stream instead of polling on every resolve call.

CREATE TABLE IF NOT EXISTS governance.policast.tags (
    entity       STRING    NOT NULL  COMMENT 'catalog.schema.table or catalog.schema.table:column',
    entity_kind  STRING    NOT NULL  COMMENT 'table | column',
    tag          STRING    NOT NULL  COMMENT 'bare tag name, e.g. pii, phi, clinical',
    set_by       STRING    NOT NULL,
    set_at       TIMESTAMP NOT NULL,
    retired_at   TIMESTAMP           COMMENT 'tombstone for retired tag assignments'
)
USING DELTA
PARTITIONED BY (tag)
TBLPROPERTIES (
    'delta.enableChangeDataFeed' = 'true'
)
COMMENT 'Entity -> tag assignments driving template expansion';

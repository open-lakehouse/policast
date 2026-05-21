-- Compiled CEL manifest. This is what engines actually consume.
-- Change Data Feed is enabled so engines can tail an invalidation stream
-- instead of polling.

CREATE TABLE IF NOT EXISTS governance.policast.manifest (
    policy_id         STRING    NOT NULL,
    cel_expression    STRING    NOT NULL,
    version           BIGINT    NOT NULL  COMMENT 'matches policies.version',
    compiled_at       TIMESTAMP NOT NULL,
    compiler_version  STRING    NOT NULL  COMMENT 'policast-core crate version',
    source_hash       STRING    NOT NULL  COMMENT 'sha256 of cedar_source'
)
USING DELTA
PARTITIONED BY (policy_id)
TBLPROPERTIES (
    'delta.enableChangeDataFeed' = 'true'
)
COMMENT 'Compiled CEL artifacts consumed by engines';

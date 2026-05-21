-- Authoritative principal -> policy mapping. The bindings table is the
-- source of truth; UC table properties (policast.applied_policies) are a
-- denormalized fast-path cache.

CREATE TABLE IF NOT EXISTS governance.policast.bindings (
    binding_id           STRING    NOT NULL,
    policy_id            STRING    NOT NULL,
    target               STRING    NOT NULL  COMMENT 'catalog.schema.table or catalog.schema.* or *',
    principal_selector   STRING    NOT NULL  COMMENT 'role:<name> | group:<name> | principal:<id> | *',
    precedence           INT       NOT NULL  DEFAULT 0,
    active_from          TIMESTAMP,
    active_to            TIMESTAMP
)
USING DELTA
COMMENT 'Principal -> policy bindings, authoritative';

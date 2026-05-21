-- Seed the healthcare POC described in the top-level README.md.
-- Matches examples/policies/manifest.json 1:1.
--
-- The column_mask_* rows are authored as tag-scoped Cedar templates —
-- target_table = '*' and applies_to_tag names the sensitivity class.
-- The resolver expands one concrete (table, column) policy per
-- matching row in governance.policast.tags at resolve time.

INSERT INTO governance.policast.policies
    (policy_id, cedar_source, filter_type, target_table, column,
     target_tag, applies_to_tag, effect, applies_to_roles, description,
     version, created_by, created_at, retired_at)
VALUES
    ('row_filter_region',
     '-- see examples/policies/row_filter.cedar',
     'row_filter', '*', NULL,
     'clinical', NULL, 'permit', ARRAY('analyst'),
     'Restrict analysts to clinical rows in their assigned region',
     1, 'governance_admin', CURRENT_TIMESTAMP(), NULL),

    ('row_filter_physician',
     '-- see examples/policies/row_filter.cedar',
     'row_filter', 'hospital.clinical.patients', NULL,
     NULL, NULL, 'permit', ARRAY('physician'),
     'Restrict physicians to their own patients',
     1, 'governance_admin', CURRENT_TIMESTAMP(), NULL),

    ('column_mask_by_pii_tag',
     '-- see examples/policies/column_mask.cedar',
     'column_mask', '*', NULL,
     NULL, 'pii', 'forbid', NULL,
     'Mask columns tagged `pii` for non-admin, non-physician users',
     1, 'governance_admin', CURRENT_TIMESTAMP(), NULL),

    ('column_mask_by_phi_tag',
     '-- see examples/policies/column_mask.cedar',
     'column_mask', '*', NULL,
     NULL, 'phi', 'forbid', NULL,
     'Mask columns tagged `phi` for non-admin, non-physician users',
     1, 'governance_admin', CURRENT_TIMESTAMP(), NULL),

    ('deny_legal_hold',
     '-- see examples/policies/deny_legal_hold.cedar',
     'deny_override', 'hospital.clinical.patients', NULL,
     NULL, NULL, 'forbid', NULL,
     'Block access to records under legal hold unless user has legal role',
     1, 'governance_admin', CURRENT_TIMESTAMP(), NULL);

INSERT INTO governance.policast.manifest VALUES
    ('row_filter_region',
     '(resource.region == principal.region)',
     1, CURRENT_TIMESTAMP(), '0.1.0', 'sha256:row_filter_region@v1'),
    ('row_filter_physician',
     '(resource.treating_physician == principal.name)',
     1, CURRENT_TIMESTAMP(), '0.1.0', 'sha256:row_filter_physician@v1'),
    ('column_mask_by_pii_tag',
     '!(((principal.role == "admin") || (principal.role == "physician")))',
     1, CURRENT_TIMESTAMP(), '0.1.0', 'sha256:column_mask_by_pii_tag@v1'),
    ('column_mask_by_phi_tag',
     '!(((principal.role == "admin") || (principal.role == "physician")))',
     1, CURRENT_TIMESTAMP(), '0.1.0', 'sha256:column_mask_by_phi_tag@v1'),
    ('deny_legal_hold',
     '(resource.legal_hold == true) && !((principal.role == "legal"))',
     1, CURRENT_TIMESTAMP(), '0.1.0', 'sha256:deny_legal_hold@v1');

INSERT INTO governance.policast.bindings VALUES
    ('b-row-region',      'row_filter_region',      'hospital.clinical.patients', 'role:analyst',    100, NULL, NULL),
    ('b-row-physician',   'row_filter_physician',   'hospital.clinical.patients', 'role:physician',  100, NULL, NULL),
    ('b-mask-pii',        'column_mask_by_pii_tag', 'hospital.clinical.patients', '*',                50, NULL, NULL),
    ('b-mask-phi',        'column_mask_by_phi_tag', 'hospital.clinical.patients', '*',                50, NULL, NULL),
    ('b-deny-legal-hold', 'deny_legal_hold',        'hospital.clinical.patients', '*',               200, NULL, NULL);

-- Denormalize bindings onto the governed table so engines can resolve
-- the applied-policy set from UC table properties in one call. The
-- bindings table remains authoritative on conflicts.
ALTER TABLE hospital.clinical.patients SET TBLPROPERTIES (
    'policast.applied_policies' = 'row_filter_region,row_filter_physician,column_mask_by_pii_tag,column_mask_by_phi_tag,deny_legal_hold',
    'policast.sensitivity'      = 'phi'
);

-- Tag assignments. These three rows are what the three tag-scoped
-- policies expand against:
--   * `clinical` on the patients table → row_filter_region fires
--     (analyst regional-isolation rule now applies to every table a
--     governance admin tags as `clinical`, not just patients)
--   * `pii` on patients.ssn → column_mask_by_pii_tag fires
--   * `phi` on patients.diagnosis → column_mask_by_phi_tag fires
-- Adding a new sensitive column — or a new clinical table — is a
-- one-row INSERT here rather than a Cedar edit plus redeploy.
INSERT INTO governance.policast.tags VALUES
    ('hospital.clinical.patients',            'table',  'clinical',
     'governance_admin', CURRENT_TIMESTAMP(), NULL),
    ('hospital.clinical.patients:ssn',        'column', 'pii',
     'governance_admin', CURRENT_TIMESTAMP(), NULL),
    ('hospital.clinical.patients:diagnosis',  'column', 'phi',
     'governance_admin', CURRENT_TIMESTAMP(), NULL);

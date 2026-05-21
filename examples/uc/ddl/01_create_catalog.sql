-- Create the reserved governance catalog and the policast schema.
-- Admin access (MANAGE, MODIFY) should be held by a governance_admin role;
-- reader access (USAGE, SELECT on specific tables) is granted to engines
-- that need to resolve policies.

CREATE CATALOG IF NOT EXISTS governance
COMMENT 'Reserved catalog for policast-cel governance state';

CREATE SCHEMA IF NOT EXISTS governance.policast
COMMENT 'Cedar policies, compiled CEL manifest, and principal bindings';

-- Raw Cedar sources live in a UC volume so they can be versioned and
-- diffed like any other blob.
CREATE VOLUME IF NOT EXISTS governance.policast.raw
COMMENT 'Raw .cedar sources, named <policy_id>@v<version>.cedar';

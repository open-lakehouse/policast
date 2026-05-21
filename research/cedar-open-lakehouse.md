# Cedar for the Open Lakehouse

## A Policy Language Deep Dive for Iceberg REST and Unity Catalog

> **Draft — The Open Lakehouse Chronicles**
> *Why a 4-year-old policy language out of AWS might be the most important piece of portable governance plumbing for the open lakehouse, and how to start using it across Apache Polaris, Lakekeeper, and Unity Catalog without picking a winner.*

---

## The governance problem nobody is solving

The open lakehouse won the format war. Iceberg and Delta are both table formats you can trust, the REST Catalog spec has turned catalogs into an interchangeable substrate, and credential vending has made storage a solved problem. The one thing we haven't unified — the thing still duct-taped together behind every real production lakehouse — is **authorization**.

Today, the engineer responsible for governance has to reason about at least three different permission vocabularies in parallel:

- **Apache Polaris** uses a two-layer RBAC model — *principal roles* stacked on *catalog roles*, binding privileges like `TABLE_READ_DATA`, `TABLE_WRITE_DATA`, and `NAMESPACE_CREATE` to securable objects (catalogs, namespaces, tables, views, and now policies).
- **Lakekeeper** ships with **OpenFGA** as its default authorization backend, borrowing Zanzibar's relationship-based model, plus an **OPA bridge** so Trino can honor the same permissions via Rego.
- **Unity Catalog** leans on a **SQL `GRANT` model** with a three-level namespace (`catalog.schema.table`), privileges like `SELECT`, `MODIFY`, `USE CATALOG`, `USE SCHEMA`, `BROWSE`, and `MANAGE`, plus ownership semantics that implicitly grant everything.

Each model is reasonable in isolation. But if your organization runs more than one — and most real lakehouses do — you get to reimplement the same access intent three times, in three different vocabularies, with three different audit trails, and three different blast radii when somebody gets a grant wrong.

This is the problem **Cedar** is suspiciously well-shaped to solve. Not because it's a silver bullet. Because of a small set of design decisions that make policy *portable, analyzable, and safe by default*, and because the catalog ecosystem is already converging on the pluggable-PDP pattern that Cedar slots into cleanly.

Let's dig in.

---

## Part 1: What Cedar actually is

Cedar is an open-source policy language and evaluation engine originally built at AWS. It's the engine behind Amazon Verified Permissions, it ships in Rust (with a Go implementation donated by StrongDM and Wasm bindings for the JS/TS ecosystem), and it's currently onboarding to the **CNCF Sandbox** — which, if you're keeping score, puts it on the same trajectory as OPA, SPIFFE, and the other infrastructure-layer authorization projects.

The core pitch is disarmingly simple:

> Express your app's permissions as separate, declarative policies. Call an engine to decide *"Is this request allowed?"*. The engine gives you back `Allow` or `Deny`, plus diagnostics. Your business logic never embeds an `if user.role == "admin"` anywhere.

Every authorization question in Cedar is a **PARC** tuple:

- **P**rincipal — who is making the request (a `User`, a `ServiceAccount`, a `Group`)
- **A**ction — what they're trying to do (`ReadTable`, `CommitSnapshot`, `CreateNamespace`)
- **R**esource — what they're acting on (a `Table`, a `Namespace`, a `View`)
- **C**ontext — ambient facts about the request (time, IP, MFA status, classification tags)

Here's "hello world":

```cedar
permit (
  principal in Group::"data-engineers",
  action == Action::"ReadTable",
  resource in Namespace::"analytics"
);
```

That reads as plain English: *any principal in the `data-engineers` group can perform `ReadTable` on any resource inside the `analytics` namespace*. Cedar's readability is not an accident — it was a stated design goal that **anyone comfortable writing SQL should be able to read and write Cedar**.

But readability is table stakes. The interesting part is *why* Cedar looks the way it does.

---

## Part 2: The design decisions that matter

If you're evaluating Cedar against OPA/Rego or OpenFGA — which you should be — the design decisions below are where Cedar earns its keep. None of them are accidental. Most were explicitly traded against expressiveness to keep the language *analyzable*.

### Decision 1: Policy is data, not code

Cedar policies have **no side effects** and **no mutation**. A policy can't call out to a database, can't update a counter, can't set a flag. It reads the PARC tuple and the entity graph, evaluates boolean expressions, and returns a decision.

That's a big deal because it means:

- Policies are **trivially parallelizable** (no shared state).
- They can be **cached, replicated, and pushed to edge PDPs** with no coordination.
- They can be **analyzed statically**, which we'll come back to.

### Decision 2: Bounded evaluation

Cedar has no loops. It has no unbounded recursion. The language's most expensive operations — `containsAll`, `containsAny` on sets — are linear in the worst case, and most operations are constant time. In the extended paper, the authors report that Cedar's authorizer is roughly 28–35× faster than OpenFGA and 42–80× faster than Rego on comparable workloads.

For a REST catalog, this matters a lot. Every `loadTable`, every `listNamespaces`, every `commitSnapshot` is a hot path. An authorization check on every single call has to fit inside the latency budget your query engine is already squeezing.

### Decision 3: Default deny, forbid overrides permit, skip on error, order independence

These four properties are worth internalizing because they're the ones that make Cedar policies *composable* in a large organization:

1. **Default deny.** An empty policy set denies everything. You don't need to write a catch-all.
2. **Forbid overrides permit.** A single matching `forbid` policy wins, no matter how many `permit` policies also match. This lets your security team write guardrails (*"no access to PII from outside the corporate network"*) that downstream teams can't accidentally override.
3. **Skip on error.** If a policy evaluation throws — say, it references an attribute that doesn't exist on this principal — that policy is ignored, not treated as a deny. Without skip-on-error, one bad policy could catastrophically lock out your whole platform.
4. **Order independence.** Policies can be evaluated in any order and you get the same answer. This is critical for distributed PDPs.

If you've written IAM policies, these properties will feel familiar — Cedar consciously inherits them. The win is that Cedar is formally *proven* to satisfy them, using the Lean theorem prover. AWS calls this approach **verification-guided development**, and it's the reason Cedar's authorization engine comes with mathematical guarantees rather than "we ran a lot of tests."

### Decision 4: Schema as optional types

Cedar schemas describe your entity types, actions, and the shape of the request context. A schema for a photo-sharing app looks roughly like this:

```cedar
namespace PhotoFlash {
  entity User in UserGroup = {
    "department": String,
    "jobLevel": Long,
  };
  entity UserGroup;
  entity Album in Album = {
    "account": Account,
    "private": Bool,
  };
  entity Photo in Album = {
    "account": Account,
    "private": Bool,
  };

  action "viewPhoto" appliesTo {
    principal: User,
    resource: Photo,
    context: { "authenticated": Bool }
  };
}
```

Two things to notice:

1. **Entities are hierarchical.** `Photo in Album` means a photo lives inside an album — which, if you squint, looks a lot like `Table in Namespace in Catalog`.
2. **Schemas validate policies, not requests.** Cedar doesn't use the schema at evaluation time. It uses it at *policy-authoring* time, to catch typos, misspellings, and type errors before you deploy. A policy that references `resource.privaet` instead of `resource.private` fails validation — you don't find out at 3am when nobody can load a table.

As of Cedar 4.3 you also get **enumerated entity types** for when you want a fixed set of principals or resources, and as of 4.5 you get the **`is` operator** for writing policies that discriminate on resource type (`resource is Table`) — which is extremely useful when you want a single catalog-level policy to match tables but not views.

### Decision 5: SMT-based policy analysis

This is the feature nobody else has. Cedar ships a **symbolic compiler** that translates policies into SMT-LIB, lets a solver reason about them, and gives you concrete counterexamples when your intent diverges from your code.

In practice this means you can ask questions like:

- *"Does my new refactored policy set allow strictly fewer requests than the old one?"* (No surprise new grants.)
- *"Is there any way a principal outside `admins` can reach a table tagged `pii`?"*
- *"Do these two policies contradict each other?"*

For a governance team working across multiple catalogs — where policy changes ripple into production tables — having a *proof* that your refactor didn't accidentally open a door is a qualitatively different operating posture than having a unit test.

---

## Part 3: The Cedar language, in under ten minutes

Cedar has three primary moving parts: **policies**, **schemas**, and **entities**. You've seen all three above. Let's tighten up the vocabulary before we go build a lakehouse model.

### Policies

A Cedar policy is an `effect`, a `scope`, optional `when`/`unless` clauses, and optional annotations:

```cedar
@id("read-analytics-as-engineer")
@description("Analysts and engineers can SELECT from anything in analytics")
permit (
  principal in Group::"data-engineers",
  action in [Action::"ReadTable", Action::"ListTables"],
  resource in Namespace::"main::analytics"
)
when {
  context.mfa == true &&
  !(resource has classification && resource.classification == "restricted")
};
```

Anatomy:

- `permit` / `forbid` — the effect.
- The scope — `principal`, `action`, `resource` — can use `==`, `in`, or `is` (for type checks).
- `when` — conditions that must hold true.
- `unless` — conditions that must hold false. (Equivalent to `when { !(...) }` but often more readable.)
- Annotations — `@id`, `@description`, and any custom `@foo` your tooling cares about.

### Templates

For RBAC-style "grant this role this permission on this resource" patterns, Cedar templates use `?principal` and `?resource` as placeholders:

```cedar
@id("grant-namespace-write")
permit (
  principal == ?principal,
  action in [Action::"CreateTable", Action::"CommitSnapshot"],
  resource in ?resource
);
```

Your catalog's management API then issues *template-linked policies* — instantiations of the template against specific principals and resources. You get the expressiveness of general policies with the operational model of row-in-a-permissions-table. This is the pattern you want for granting per-user, per-resource access; it's what AWS Verified Permissions uses under the hood.

### Entities

Entities are the data Cedar reasons over. For a request to evaluate, you pass the engine an *entity store* — usually a JSON file or in-memory structure — that contains all the principals, resources, and their parents and attributes:

```json
[
  {
    "uid": { "type": "User", "id": "scott" },
    "parents": [{ "type": "Group", "id": "data-engineers" }],
    "attrs": { "department": "developer-relations" }
  },
  {
    "uid": { "type": "Table", "id": "main.analytics.events" },
    "parents": [{ "type": "Namespace", "id": "main.analytics" }],
    "attrs": { "classification": "internal" }
  }
]
```

The `parents` field is what makes `in` work in policies. When you ask *"is `scott` in `Group::"data-engineers"`?"*, Cedar walks the parent chain. For a lakehouse, this is exactly what you want: `Table` has parent `Namespace`, `Namespace` has parent `Catalog`, and privileges cascade.

---

## Part 4: The catalog landscape, honestly

Before we try to model a lakehouse in Cedar, we need to be honest about what the three main catalogs actually give us to work with. Because "plug Cedar in" is a different exercise for each one.

### Apache Polaris

Polaris is the youngest of the three, donated to Apache by Snowflake and currently in the Incubator. Its authorization model is a classic two-tier RBAC:

```
Principal ──(many)→ PrincipalRole ──(many)→ CatalogRole ──(many)→ Privilege
```

A **CatalogRole** holds privilege grants on securable objects. Securable objects include catalogs, namespaces, tables, views, and — as of Polaris 1.0 — **policies** themselves. Privileges look like `TABLE_READ_DATA`, `TABLE_WRITE_DATA`, `NAMESPACE_CREATE`, and in 1.2 got finer-grained (you can now grant the specific operation instead of the broad `TABLE_WRITE_PROPERTIES`).

The critical development for this post is **Polaris 1.3**, released in January 2026, which added **Open Policy Agent integration** via `polaris.authorization.type=opa`. This is the pluggable-PDP pattern landing in the reference Apache Iceberg REST catalog. It means you can **delegate authorization decisions to an external policy decision point** — and that PDP doesn't have to be OPA. It just has to speak the interface Polaris expects.

Polaris also has a separate **Policy Store** concept — a CRUD API for policies attached to catalogs, namespaces, tables, and views. Today those policies are for data lifecycle concerns (compaction, snapshot expiry, orphan file cleanup). The roadmap calls for FGAC — fine-grained access control — policies in the same store.

### Lakekeeper

Lakekeeper is a Rust-native Iceberg REST catalog that ships with **OpenFGA** as its default authorization system. OpenFGA gives you ReBAC — Google Zanzibar-style relationship tuples like `table:events#viewer@user:scott`. The permission model has bi-directional inheritance, which matches the hierarchical namespace model in modern lakehouses.

Two things make Lakekeeper the most Cedar-friendly catalog today:

1. **It's written in Rust.** Cedar ships as a first-class Rust crate. There is no SDK impedance mismatch.
2. **It has a pluggable `Authorizer` trait.** From the Lakekeeper README: *"If your company already has a different system in place, you can integrate with it by implementing a handful of methods in the Authorizer trait."* A Cedar-backed `Authorizer` implementation is a weekend project, not a fork.

Lakekeeper also has an **OPA bridge** specifically for Trino, which exposes its OpenFGA permissions through an OPA-compatible interface. The same bridge pattern works for Cedar.

### Unity Catalog

Unity Catalog (both the commercial Databricks offering and the open-source version) uses a **SQL GRANT model** over a three-level namespace. The securable-object set is much richer than Polaris or Lakekeeper — it includes not just catalogs, schemas, tables, and views, but also `VOLUME`, `FUNCTION`, `CONNECTION`, `EXTERNAL LOCATION`, `EXTERNAL METADATA`, `SERVICE CREDENTIAL`, `STORAGE CREDENTIAL`, `CLEAN ROOM`, and more.

Privileges include the familiar `SELECT`, `MODIFY`, `USE CATALOG`, `USE SCHEMA`, `CREATE TABLE`, plus the interesting ones: `BROWSE` (see that an object exists without being able to read it), `MANAGE` (control permissions without owning), and `APPLY TAG` (a prerequisite for most tag-based FGAC).

Ownership is special: the object owner has all privileges implicitly, and ownership doesn't inherit downward (owning a catalog doesn't mean you own its schemas, but you *can* manage their permissions).

Unity Catalog OSS exposes its permission management via a CLI and REST API:

```bash
bin/uc permission create \
  --securable_type catalog \
  --name unity \
  --privilege 'USE CATALOG' \
  --principal scott@example.com
```

Unlike Polaris 1.3, Unity Catalog does not (yet) have a first-class "delegate authorization to an external PDP" hook. The integration points are the REST API and the SQL DDL surface.

### The convergence point

Pattern-match across the three:

| Catalog | Native Model | External PDP Hook | Best Cedar On-Ramp |
|---|---|---|---|
| Apache Polaris | RBAC (2-tier) | **OPA (1.3+)** | Replace OPA with Cedar at the same interface |
| Lakekeeper | OpenFGA + OPA bridge | Pluggable `Authorizer` trait (Rust) | Implement `Authorizer` with `cedar-policy` crate |
| Unity Catalog | SQL GRANT model | None yet | Compile Cedar policies → GRANT statements, or wrap the REST API |

The three paths are different, but they share a common target: **one Cedar schema, one set of policies, three execution strategies**. The next part shows how to build that schema.

---

## Part 5: Modeling a lakehouse in Cedar

Here's the portable schema. It's designed to cover the union of what Polaris, Lakekeeper, and Unity Catalog can express, without being specific to any of them.

```cedar
namespace Lakehouse {

  // --- Principals ---
  entity User in [Group] = {
    "email": String,
    "department": String,
  };

  entity ServiceAccount in [Group] = {
    "owner": User,
  };

  entity Group in [Group] = {
    "name": String,
  };

  // --- Resources (the catalog hierarchy) ---
  entity Catalog = {
    "name": String,
    "type": String, // "internal" | "external" | "federated"
  };

  entity Namespace in [Namespace, Catalog] = {
    "name": String,
  };

  entity Table in [Namespace] = {
    "name": String,
    "classification": String,  // "public" | "internal" | "pii" | "restricted"
    "owner": User,
    "format": String,          // "iceberg" | "delta"
  };

  entity View in [Namespace] = {
    "name": String,
    "classification": String,
    "owner": User,
  };

  // --- Actions ---
  // Namespace-scoped
  action "CreateNamespace", "DropNamespace", "ListNamespaces"
    appliesTo {
      principal: [User, ServiceAccount],
      resource: Catalog,
      context: { "mfa": Bool, "network": String }
    };

  // Table-scoped
  action "CreateTable", "DropTable", "RenameTable", "ListTables"
    appliesTo {
      principal: [User, ServiceAccount],
      resource: Namespace,
      context: { "mfa": Bool, "network": String }
    };

  action "ReadTable", "WriteTable", "CommitSnapshot", "LoadTableMetadata"
    appliesTo {
      principal: [User, ServiceAccount],
      resource: [Table, View],
      context: {
        "mfa": Bool,
        "network": String,
        "purpose": String
      }
    };

  // Admin-scoped
  action "ManageGrants", "TransferOwnership"
    appliesTo {
      principal: [User],
      resource: [Catalog, Namespace, Table, View],
      context: { "mfa": Bool }
    };
}
```

A few notes on why this shape:

1. **Namespace is self-referential** (`Namespace in [Namespace, Catalog]`). Polaris supports nested namespaces up to 16 levels; Unity Catalog's three-level namespace is a degenerate case of this. The recursive parent relationship handles both.
2. **Tables carry a `classification` attribute.** This is where ABAC enters. Whether it comes from UC tags, Polaris policies, or a sidecar metadata service, the policy engine just needs to see it on the entity.
3. **Actions are unioned across catalogs.** `LoadTableMetadata` maps to Polaris's `TABLE_READ_PROPERTIES`, UC's implicit metadata read, and an Iceberg REST `loadTable` call. A deployment only needs to map the subset of actions its catalog actually distinguishes.
4. **Context includes MFA, network, and purpose.** These are the ambient facts that turn a static RBAC model into ABAC — and they're what every real governance review asks for.

---

## Part 6: Portable policies

With that schema in place, you can write policies that cover every common governance pattern. Here's a progression from RBAC to ABAC to ReBAC to cross-cutting guardrails.

### RBAC: classic role-based read access

```cedar
@id("p1-data-engineers-read-analytics")
permit (
  principal in Lakehouse::Group::"data-engineers",
  action in [
    Lakehouse::Action::"ReadTable",
    Lakehouse::Action::"LoadTableMetadata",
    Lakehouse::Action::"ListTables"
  ],
  resource in Lakehouse::Catalog::"main"
);
```

Equivalent in each native system:
- **Polaris**: A `data-engineers` principal role bound to a catalog role that holds `TABLE_READ_DATA` + `TABLE_READ_PROPERTIES` on `main`.
- **Unity Catalog**: `GRANT USE CATALOG ON CATALOG main TO \`data-engineers\`; GRANT SELECT ON CATALOG main TO \`data-engineers\`;`
- **Lakekeeper**: An OpenFGA tuple `catalog:main#select@group:data-engineers`.

One Cedar policy; three native emissions. That's the portability story.

### ABAC: classification-gated access

```cedar
@id("p2-no-pii-without-purpose")
forbid (
  principal,
  action == Lakehouse::Action::"ReadTable",
  resource
)
when {
  resource has classification &&
  resource.classification == "pii"
}
unless {
  context has purpose &&
  context.purpose in ["analytics-approved", "ml-training-approved"]
};
```

This is the kind of policy that's awkward in every native system — Polaris privileges are resource-bound, UC tags require separate row/column-level security policies, OpenFGA doesn't do attributes natively. In Cedar it's five lines, and because it's a `forbid`, it **cannot be overridden** by any `permit` elsewhere. That's your guardrail.

### ReBAC: ownership

```cedar
@id("p3-owners-can-manage")
permit (
  principal,
  action in [
    Lakehouse::Action::"ManageGrants",
    Lakehouse::Action::"TransferOwnership",
    Lakehouse::Action::"DropTable"
  ],
  resource
)
when {
  resource has owner && resource.owner == principal
};
```

Ownership-as-attribute gives you UC's ownership semantics without the special-case logic baked into the engine. Same model works for Polaris (where ownership is currently less privileged than admin) and Lakekeeper (where "owners of objects have all rights on the specific object" is a core model principle).

### Cross-cutting: network egress guardrail

```cedar
@id("p4-restricted-from-corp-only")
forbid (
  principal,
  action in [
    Lakehouse::Action::"ReadTable",
    Lakehouse::Action::"WriteTable"
  ],
  resource
)
when {
  resource has classification &&
  resource.classification == "restricted" &&
  context.network != "corporate"
};
```

One policy, applies to every catalog, enforces a company-wide rule.

### Template: per-table grant (the UC/Polaris bread-and-butter)

```cedar
@id("tpl-grant-read")
permit (
  principal == ?principal,
  action in [
    Lakehouse::Action::"ReadTable",
    Lakehouse::Action::"LoadTableMetadata"
  ],
  resource == ?resource
);
```

Every `GRANT SELECT ON TABLE x TO user y` in UC, or every `TABLE_READ_DATA` assigned to a catalog role in Polaris, is a template-linked instantiation of this single policy.

---

## Part 7: Integration patterns

There are three plausible ways to wire Cedar into an Iceberg REST catalog or Unity Catalog deployment. They have real trade-offs.

### Pattern A: Embedded PDP (Cedar in-process)

The catalog service links the `cedar-policy` Rust crate directly. On every request, before performing the operation, it calls `PolicySet::is_authorized(...)`.

```rust
use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityUid,
    PolicySet, Request, Schema,
};

pub struct CedarAuthorizer {
    policies: PolicySet,
    schema: Schema,
    authorizer: Authorizer,
}

impl CedarAuthorizer {
    pub fn check(
        &self,
        principal: EntityUid,
        action: EntityUid,
        resource: EntityUid,
        context: Context,
        entities: &Entities,
    ) -> Result<(), AuthzError> {
        let request = Request::new(
            principal,
            action,
            resource,
            context,
            Some(&self.schema),
        )?;
        let response = self.authorizer.is_authorized(&request, &self.policies, entities);
        match response.decision() {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(AuthzError::Denied(
                response.diagnostics().reason().cloned().collect()
            )),
        }
    }
}
```

**Pros:** Sub-millisecond evaluation. No network hop. The entire policy set lives next to the catalog, which is exactly where your request-handling code is already making decisions.

**Cons:** Entity data has to be available in-process. For Lakekeeper, this is easy — the catalog already has the namespace/table graph. For Unity Catalog, where the metastore is the source of truth, you'd need to snapshot or stream the entity graph into the PEP. The freshness question — *"when a table's classification tag changes, how long until my policies see it?"* — is real.

**Best for:** Lakekeeper (and any Rust-native REST catalog you're building yourself).

### Pattern B: Sidecar PDP

Run Cedar as a separate service — its own process, its own HTTP or gRPC endpoint. The catalog calls out on every decision. This is exactly the shape of the Polaris 1.3 OPA integration, and a Cedar PDP can drop into the same slot.

**Pros:** Language-agnostic. Polaris (Java/Quarkus) doesn't have to link Rust. Your security team owns and operates the PDP independently. You can scale the PDP separately from the catalog.

**Cons:** One extra network hop per request. For a list operation that touches 10,000 tables, that matters. The `PolicySet::is_authorized_batch` API (and the `tpe` feature flag for **targeted partial evaluation**, which replaced the old entity-manifest feature in recent Cedar releases) exist specifically to amortize this cost — you send one call with many (principal, action, resource) triples.

**Best for:** Polaris, or any polyglot deployment where the catalog isn't Rust.

### Pattern C: Compile Cedar to native grants

Treat Cedar as the *source of truth* but compile policies down to native grants at deploy time. Your Cedar policy `permit(principal in Group::"data-engineers", action == Action::"ReadTable", resource in Namespace::"analytics")` becomes:

- A Unity Catalog `GRANT SELECT ON SCHEMA main.analytics TO \`data-engineers\`;`
- A Polaris API call that assigns `TABLE_READ_DATA` to a catalog role and binds it to the matching principal role.
- An OpenFGA tuple-write in Lakekeeper.

**Pros:** Zero runtime overhead. The native enforcement point is already battle-tested. Works with Unity Catalog today, without any PDP hook.

**Cons:** Only expressible policies can be compiled — ABAC with context (network, MFA, purpose) can't reduce to static grants in most native systems. You end up with a bimodal deployment: RBAC compiles to grants, ABAC lives in a PDP. That's not necessarily bad, but it means your "one policy language" story has two delivery paths.

**Best for:** Unity Catalog, or as a migration path when you want Cedar as your system of record but can't yet plug in a PDP.

### The entity freshness problem

Whichever pattern you pick, you have to answer: *where does the entity graph live, and how fresh is it?* Cedar needs to know that `Table::"main.analytics.events" has parent Namespace::"main.analytics"`, and it needs to know the table's classification. Three common approaches:

1. **Catalog-sourced** (preferred for lakehouses). The catalog itself is the source of truth. The PDP reads from the catalog's entity store on demand, or subscribes to change events (Polaris already emits CloudEvents; Lakekeeper has a change events feature) and keeps a local cache.
2. **Snapshot-per-request.** Pack the relevant entities into every authorization call. Works for small graphs, breaks down quickly.
3. **Dedicated entity store.** Run `cedar-local-agent` (a configurable cache for Cedar policies and entities) alongside the PDP. Good for high-throughput, eventually-consistent deployments.

For a production lakehouse, the catalog is the authoritative metadata store. Don't duplicate. Subscribe to change events and cache.

---

## Part 8: A concrete Iceberg REST PEP in Rust

Here's a minimal, illustrative Policy Enforcement Point for Iceberg REST Catalog operations. It's written against the `cedar-policy` Rust crate (currently on the 4.x series), and it maps the Iceberg REST operations that actually matter into Cedar actions.

```rust
use axum::{extract::Path, http::StatusCode, Json};
use cedar_policy::{
    Authorizer, Context, Decision, EntityUid, PolicySet, Request, Schema,
};
use std::str::FromStr;
use std::sync::Arc;

pub struct LakehousePep {
    policies: PolicySet,
    schema: Schema,
    authorizer: Authorizer,
    entities_client: Arc<dyn EntitiesClient>, // pulls live entity graph from the catalog
}

impl LakehousePep {
    pub fn new(
        policies_src: &str,
        schema_src: &str,
        entities_client: Arc<dyn EntitiesClient>,
    ) -> anyhow::Result<Self> {
        let policies = PolicySet::from_str(policies_src)?;
        let (schema, _warnings) = Schema::from_cedarschema_str(schema_src)?;
        Ok(Self {
            policies,
            schema,
            authorizer: Authorizer::new(),
            entities_client,
        })
    }

    pub async fn authorize_load_table(
        &self,
        caller_sub: &str,
        namespace: &[String],
        table: &str,
        request_ctx: RequestContext,
    ) -> Result<(), StatusCode> {
        let principal = EntityUid::from_str(
            &format!(r#"Lakehouse::User::"{}""#, caller_sub)
        ).map_err(|_| StatusCode::BAD_REQUEST)?;

        let action = EntityUid::from_str(
            r#"Lakehouse::Action::"LoadTableMetadata""#
        ).unwrap();

        let table_id = format!("{}.{}", namespace.join("."), table);
        let resource = EntityUid::from_str(
            &format!(r#"Lakehouse::Table::"{}""#, table_id)
        ).map_err(|_| StatusCode::BAD_REQUEST)?;

        let context = Context::from_pairs(
            [
                ("mfa".into(), request_ctx.mfa.into()),
                ("network".into(), request_ctx.network.into()),
                ("purpose".into(), request_ctx.purpose.unwrap_or_default().into()),
            ],
            Some(&self.schema),
        ).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        // Pull relevant entities from the catalog: the user and their groups,
        // the table and its parent namespace/catalog chain.
        let entities = self.entities_client
            .entities_for(&principal, &resource)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let request = Request::new(
            principal, action, resource, context, Some(&self.schema),
        ).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let response = self.authorizer.is_authorized(&request, &self.policies, &entities);
        match response.decision() {
            Decision::Allow => Ok(()),
            Decision::Deny => {
                // Log response.diagnostics() for audit; return 403.
                Err(StatusCode::FORBIDDEN)
            }
        }
    }
}
```

The same PEP, wrapped behind an HTTP handler, is what Polaris's OPA integration plugs into — just with Cedar's decision shape instead of OPA's. And the same PEP, implementing the Lakekeeper `Authorizer` trait, replaces OpenFGA entirely.

One pattern worth calling out: in production you almost always want **`is_authorized_batch`** for list operations. An Iceberg `listTables` that returns 5,000 tables shouldn't fire 5,000 individual authorization calls. Cedar's batch API plus targeted partial evaluation (`tpe`) lets you ask "which of these 5,000 resources is this principal allowed to read?" as a single evaluation.

---

## Part 9: The honest trade-off matrix

Cedar is great. It is not a silver bullet. Here's when it's the right tool and when it isn't.

### Cedar vs. OPA/Rego for catalog governance

OPA has more momentum in the cloud-native ecosystem, it's a CNCF graduated project, and Polaris 1.3 ships with it. Rego is Turing-complete and can express just about anything. Cedar intentionally isn't — and that's the whole point. The trade-off:

- **Rego is more expressive; Cedar is more analyzable.** Rego can do things Cedar can't (recursive graph walks, aggregations). Cedar can prove things Rego can't (equivalence, non-interference).
- **Rego has a richer ecosystem today; Cedar has better performance.** OpenFGA has 10k+ stars and a big community. Cedar is ~28–35× faster than OPA on comparable workloads (per the Cedar paper), and 42–80× faster than Rego.
- **Rego is a general-purpose language; Cedar is purpose-built for authorization.** If your governance needs include compliance evaluation, data validation, admission control, *and* authorization, Rego might be the only tool that covers all of them. If your need is *just* authorization, Cedar is simpler and safer.

For a portable governance story across Polaris/Lakekeeper/Unity Catalog, I think Cedar wins on three axes: the formal guarantees (forbid overrides permit, order independence) are properties you actually want in a cross-catalog setting; the schema-based validation catches errors in a domain where misconfigurations are expensive; and the SMT analysis gives you a way to refactor without fear.

### Cedar vs. OpenFGA

OpenFGA is ReBAC-first — tuples, relationships, graph traversal. For "who can access what" questions over a sharing graph, it's purpose-built. For ABAC-style "who, under what conditions, in what context" questions, it's awkward. Lakekeeper's choice of OpenFGA reflects the former — hierarchical namespaces are fundamentally a sharing graph.

Cedar covers both RBAC and ABAC cleanly, plus the ReBAC patterns you'd use OpenFGA for (via the entity hierarchy and `in` operator). The trade-off: OpenFGA's tuple store is a first-class system. Cedar's entity store is a data structure you pass to the engine. If you're managing millions of fine-grained relationships, OpenFGA's store might be more operationally mature.

### What you give up with portability

Being honest: a Cedar-based portable policy layer can't express every native feature. You lose:

- **UC's row-level and column-level security policies** — these are compiled into the query plan by the engine, not by the catalog.
- **UC's dynamic views and masking functions** — again, engine-level.
- **Polaris's per-table policy-store entries** — at least until Polaris exposes them via the OPA interface.
- **Lakekeeper's batch permission check semantics** — unless you implement them behind your PDP.

Cedar handles **coarse and medium-grained access control**: *who can see this catalog, who can load metadata for this table, who can write to this namespace*. For finer-grained enforcement (row, column, cell), you'll still need the engine's native features — but you can use Cedar to decide *whether the user gets a connection at all*, and let the engine handle the rest.

---

## Part 10: A minimal starter project

If you want to run this end-to-end in an afternoon:

1. `cargo new --bin lakehouse-pdp` and add `cedar-policy = "4"` to your dependencies.
2. Save the `Lakehouse` namespace schema above as `schema.cedarschema`.
3. Save the four example policies as `policies.cedar`.
4. Validate: `cedar validate --schema schema.cedarschema --policies policies.cedar`.
5. Author a handful of entities in JSON matching your local Polaris or Lakekeeper — a few users, groups, namespaces, tables.
6. Run `cedar authorize --policies policies.cedar --entities entities.json --schema schema.cedarschema --principal 'Lakehouse::User::"scott"' --action 'Lakehouse::Action::"ReadTable"' --resource 'Lakehouse::Table::"main.analytics.events"'` with various context values and watch the allow/deny flip.
7. Wire the same `PolicySet` into an Axum or Actix HTTP handler that sits in front of a Lakekeeper instance (or behind Polaris's OPA hook, which accepts a similar protocol).

Then, separately:

- Install `cedar-policy-symcc` and try the symbolic analysis. Ask "is there any request where a `User` not in `data-engineers` can `ReadTable` on a resource in `Namespace::"main.analytics"`?" — the solver will either prove no such request exists, or hand you a counterexample.

That last step is what will convince your security team. Not the Rust, not the speed, not the readability. The fact that you can **prove invariants** about your access model.

---

## Closing

The open lakehouse's governance fragmentation isn't going to resolve on its own. The catalogs won't converge on a single privilege model — Unity Catalog's three-level namespace with SQL GRANTs, Polaris's principal/catalog role tiering, and Lakekeeper's OpenFGA ReBAC all reflect legitimately different design philosophies, and all three will keep shipping on their own timelines.

What *is* converging is the pluggable-PDP pattern. Polaris 1.3 shipping OPA integration is the signal that the reference Iceberg REST catalog now expects externalized authorization. Lakekeeper's `Authorizer` trait has invited it from day one. Unity Catalog has the REST surface and the GRANT DDL to compile against. The shape of the future is clear: one policy language, one policy set, many enforcement points.

Cedar is the best candidate I've seen for that one policy language, for three reasons that matter in practice:

1. **It reads like SQL.** The people who write your governance reviews can read and audit Cedar policies. They can't read Rego, and they definitely can't read OpenFGA DSL.
2. **It's formally verified and SMT-analyzable.** You can refactor a 500-policy set and prove you didn't change behavior. That's not a feature — that's a qualitatively different relationship with your access control.
3. **It runs in Rust, at milliseconds, with bounded latency.** The catalog hot path can afford it. No other general-purpose policy language clears that bar.

It's early. Polaris has chosen OPA first. Lakekeeper defaults to OpenFGA. Unity Catalog doesn't yet have a PDP hook. But the trajectory — CNCF sandbox, Kubernetes admission integration, MongoDB and Cloudflare already in production — puts Cedar on a path where it will be present in every layer of the stack *except* the catalog. Closing that gap is a short, useful project, and the engineers who build the bridges first will own the pattern for the next decade.

If you're building or operating a multi-catalog open lakehouse, the question isn't whether to externalize authorization. Polaris already made that decision for you. The question is what language you externalize it *in*. Cedar is worth a hard look.

---

## Further reading

- [Cedar: A New Language for Expressive, Fast, Safe, and Analyzable Authorization (Cutler et al., 2024)](https://arxiv.org/abs/2403.04651) — the extended paper
- [Cedar docs](https://docs.cedarpolicy.com/) — reference for the 4.x language
- [Cedar policy language GitHub](https://github.com/cedar-policy/cedar) — Rust implementation
- [Cedar symbolic compiler](https://github.com/cedar-policy/cedar/tree/main/cedar-policy-symcc) — SMT-based analysis
- [Apache Polaris 1.3 release notes](https://www.snowflake.com/en/engineering-blog/apache-polaris-1-3-release/) — OPA integration landing
- [Lakekeeper authorization docs](https://docs.lakekeeper.io/docs/latest/authorization-openfga/) — OpenFGA model + OPA bridge
- [Unity Catalog privileges reference](https://docs.databricks.com/aws/en/data-governance/unity-catalog/manage-privileges/privileges) — the full GRANT vocabulary
- [cedar-local-agent](https://github.com/cedar-policy/cedar-local-agent) — local caching for Cedar policies and entities

---

*Draft — comments, edits, and contradictions welcome. Especially from folks building cross-catalog governance in anger.*

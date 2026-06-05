# Policy Manifests as Protobuf

## An RFC for moving the policast-cel exchange format from JSON to Protobuf (and, later, gRPC/connectRPC)

> **Draft RFC — The Open Lakehouse Chronicles**
> *The `PolicyManifest` is the contract between a Cedar→CEL compiler written in Rust and enforcement engines written in Rust (DataFusion) and Scala/JVM (Spark). Today that contract is "some JSON we hand-maintain a struct for, twice." This RFC asks whether a Protobuf schema — the same primitive that makes gRPC/connectRPC/protovalidate "just work" — should become the source of truth, and lays out a phased, wire-compatible path to get there.*
>
> Tracking issue: **#9 — Policy Manifests as Protobuf**. This issue is explicitly an *investigation*; the deliverable is this design, not a migration.

---

## TL;DR

- The manifest is **JSON everywhere**: serde_json in three Rust crates, Gson in the Spark plugin, JSON-over-HTTP between the resolver sidecar and engines, and JSON inside the Redis cache and the HMAC signing path.
- The type contract is **hand-duplicated** in Rust and Scala, and it has **already drifted**: the Scala `CompiledPolicy` is missing `target_tag` and `applies_to_tag`, and treats `effect`/`filter_type` as raw strings with no validation. An unknown `filter_type` **silently becomes a row filter** on the Rust side too.
- Protobuf fixes the *structural* problems (one schema, generated types, enum guards, breaking-change CI) and gives smaller/faster payloads — but `cel_expression` stays an opaque string either way, and proto's **non-canonical serialization** complicates the existing "sign the canonical bytes" scheme.
- **Recommendation:** adopt a `.proto` as the source of truth now, generate types and keep **JSON on the wire** first (proto3 JSON mapping), then add **binary** as a file format and content-type, and only then consider **connectRPC** for the resolver. Sign an **envelope over exact bytes** to avoid canonicalization headaches. Do *not* boil the ocean: gRPC/connect is Phase 3, not day one.

---

## Part 1: The problem — where JSON lives today

The handoff format is the `PolicyManifest`. Here is the entire surface area it touches, traced through the codebase.

### 1.1 The compiler (`policast-core`)

The canonical types are Rust structs with `#[derive(Serialize, Deserialize)]`:

- `PolicyManifest { version: String, policies: Vec<CompiledPolicy>, principal_contract: Option<PrincipalContract> }` — `policy_manifest.rs`. Serialization is literally `serde_json::to_string_pretty` / `serde_json::from_str` in `to_json` / `from_json`.
- `CompiledPolicy { id, effect, filter_type, target_table, column?, target_tag?, applies_to_tag?, cel_expression, applies_to?, description? }` — `model.rs`.
- `Effect` (`permit`/`forbid`, lowercased) and `FilterType` (`row_filter`/`column_mask`/`deny_override`, snake_cased) are Rust enums serialized as strings.
- `AppliesTo { roles, principals }`, `PrincipalContract { required_attributes }`.

The optional fields use `#[serde(skip_serializing_if = "Option::is_none")]`, so the JSON shape is **version-dependent**: a manifest with no tags serializes identically to a pre-tags manifest. That is deliberate (older consumers keep parsing) but it means "the schema" is implicit in serde attributes rather than written down anywhere.

### 1.2 The policy store / cache (`policast-core::policy_store`)

- `PolicyQuery` and `ResolvedPolicies` are serde types.
- `FileManifestStore::from_path` does `std::fs::read_to_string` → `PolicyManifest::from_json`. The on-disk artifact is `examples/policies/manifest.json`.
- `RedisCache` stores `ResolvedPolicies` as **JSON strings** (`serde_json::to_string` / `from_str`) under human-readable keys.

### 1.3 The resolver wire types (`policast-uc`)

- `types.rs`: `Principal`, `PrincipalAttrs(BTreeMap)`, `ResolveRequest`, `ResolveBundle`, `StorageCredentials` — all serde. **`ResolveBundle` embeds a full `PolicyManifest`.**
- `client.rs`: the HTTP client POSTs with reqwest `.json(req)` and decodes `.json::<ResolveBundle>()`. So the network protocol is **JSON over HTTP**.
- `sidecar.rs`: the Axum server takes `Json<ResolveRequest>` and returns `Json<ResolveBundle>` on `POST /policies/resolve`.
- `signature.rs`: HMAC-SHA256 is computed over **`serde_json::to_vec` of the bundle with `signature` zeroed**. This is the single most important detail for any binary migration — see §4.4.

### 1.4 The Spark engine (`policast-spark`)

- `PolicyManifest.scala` re-declares the entire model as Scala `case class`es and parses with **Gson**.
- This parallel definition has **already drifted from the Rust source of truth**:
  - `CompiledPolicy` (Scala) has **no `target_tag` and no `applies_to_tag`** fields. Tag-scoped policies are expanded resolver-side today, so Spark "gets away with it" — but the contract is silently lossy, and any direct-manifest path on the JVM would drop those fields.
  - `effect` and `filter_type` are **raw `String`s** compared with `==` ("row_filter", "column_mask", …). No enum, no validation, no exhaustiveness.

### 1.5 The type-safety gaps, named

1. **Two hand-maintained copies of the schema** (Rust serde structs + Scala case classes) with **no mechanism to keep them in sync**. The drift above is not hypothetical; it is in `main`.
2. **Stringly-typed enums.** Both `effect` and `filter_type` cross the wire as bare strings. On the Rust side an unrecognized `filter_type` is silently coerced to `RowFilter` (`_ => FilterType::RowFilter` in `compile_single_policy`); on the Scala side an unrecognized value just never matches any branch. A typo degrades enforcement **silently** — the worst failure mode for a governance system.
3. **No schema artifact.** "The schema" is whatever serde + Gson happen to agree on at runtime. There is nothing to lint, diff, or run breaking-change detection against in CI.
4. **Optionality is implicit.** `skip_serializing_if` means absent-vs-empty is encoded by omission, and consumers must each re-derive the rules.
5. **Canonicalization is load-bearing but fragile.** HMAC signing depends on `serde_json` producing stable bytes (and on `BTreeMap` for ordered maps). It works, but it couples security to a serializer's formatting behavior.

`cel_expression` is, and will remain, an opaque string — CEL is a text language. Protobuf does **not** fix that; no encoding does. It is called out here so we are honest about what changes and what does not.

---

## Part 2: Why Protobuf, specifically

This project's research README already states the bias plainly: the author worked at Buf, learned Connect/protovalidate, and sees "protobuf, gRPC/connectRPC, and protovalidate as a set of concrete ingredients for crafting incredibly reliable distributed systems." CEL itself entered this project through that same protobuf lineage (protovalidate). So Protobuf is not a foreign import here — it is the substrate the rest of the design philosophy already assumes.

Concretely, Protobuf buys:

- **One schema, many languages.** A single `.proto` generates Rust (prost) and JVM (ScalaPB / protobuf-java) types. The Rust↔Scala drift becomes a *compile error in codegen*, not a silent runtime lossy field.
- **Real enums.** `Effect` and `FilterType` become closed enums with an explicit `*_UNSPECIFIED = 0` zero value, so "unknown" is *detectable* and engines can **fail closed** instead of defaulting to a row filter.
- **Breaking-change detection.** `buf breaking` in CI makes "you renamed a field" or "you reused a field number" a red build, which is exactly the guardrail a governance contract wants.
- **Smaller, faster payloads.** Binary protobuf is typically materially smaller and faster to parse than pretty-printed JSON — relevant because resolution sits on the **query hot path** and is already cached/latency-sensitive (see the Redis layer).
- **A natural service contract.** The resolver already *is* an RPC (`POST /policies/resolve`). A `service PolicyResolver` makes that contract explicit and generates clients for both engines.

What Protobuf does **not** buy: validating CEL semantics, eliminating the need for HMAC signing, or making the payload human-readable (binary is opaque — though proto3 JSON and Connect's JSON mode give that back when you want it).

---

## Part 3: A proposed `.proto` schema

A complete draft lives at [`proto/policast/v1/policast.proto`](../proto/policast/v1/policast.proto) (intentionally **not** wired into any build). The core of it:

```proto
syntax = "proto3";
package policast.v1;

enum Effect {
  EFFECT_UNSPECIFIED = 0;
  EFFECT_PERMIT = 1;
  EFFECT_FORBID = 2;
}

enum FilterType {
  FILTER_TYPE_UNSPECIFIED = 0;   // guard: today an unknown value
  FILTER_TYPE_ROW_FILTER = 1;    // silently becomes a row_filter
  FILTER_TYPE_COLUMN_MASK = 2;
  FILTER_TYPE_DENY_OVERRIDE = 3;
}

message AppliesTo {
  repeated string roles = 1;
  repeated string principals = 2;
}

message PrincipalContract {
  repeated string required_attributes = 1;  // sorted + de-duped
}

message CompiledPolicy {
  string id = 1;                      // Cedar @id
  Effect effect = 2;
  FilterType filter_type = 3;         // Cedar @filter_type
  string target_table = 4;            // Cedar @target_table; "*" / "a.b.*" ok
  optional string column = 5;
  optional string target_tag = 6;
  optional string applies_to_tag = 7;
  string cel_expression = 8;          // opaque CEL text (unchanged)
  optional AppliesTo applies_to = 9;
  optional string description = 10;
}

message PolicyManifest {
  string version = 1;                          // author-facing policy-set version
  repeated CompiledPolicy policies = 2;
  optional PrincipalContract principal_contract = 3;
  uint32 manifest_schema_version = 4;          // wire/schema version (new)
}
```

And the resolver contract, with the key design move — **signing lives on an envelope over exact bytes**:

```proto
message Principal {
  string id = 1;
  string role = 2;
  map<string, string> attrs = 3;
}

message ResolveRequest {
  string table = 1;
  Principal principal = 2;
  string requested_action = 3;   // default "query"
}

message StorageCredentials {
  optional string aws_access_key_id = 1;
  optional string aws_secret_access_key = 2;
  optional string aws_session_token = 3;
  optional string expiration = 4;
  map<string, string> extra = 5;   // was serde(flatten)
}

message ResolveBundle {              // note: no `signature` field here
  string table_uuid = 1;
  PolicyManifest compiled_manifest = 2;
  repeated string bindings_applied = 3;
  map<string, string> expanded_from = 4;
  map<string, string> identity_claims = 5;
  optional StorageCredentials storage_credentials = 6;
  optional string storage_uri = 7;
  string expires_at = 8;            // RFC3339
}

message SignedResolveBundle {
  bytes bundle_bytes = 1;           // exact serialized ResolveBundle
  string signature = 2;             // "hmac-sha256:<hex>" over bundle_bytes
}

service PolicyResolver {
  rpc Resolve(ResolveRequest) returns (SignedResolveBundle);
}
```

### Schema design notes

- **Annotations → fields.** The Cedar annotations that drive the manifest (`@id`, `@filter_type`, `@target_table`, plus `@column`/`@target_tag`/`@applies_to_tag`/`@description`/`@roles`) map cleanly onto scalar fields and the `AppliesTo` message. The annotations themselves are *compile-time inputs*; the manifest is the *compiled output*, so there is no need for a generic annotation map.
- **`optional` is deliberate.** Every field that is `Option<T>` in Rust uses proto3 `optional` so "absent" stays distinct from "empty default" — preserving the current `skip_serializing_if` semantics exactly.
- **Field names stay snake_case** to match the existing JSON keys, which is what makes the JSON-on-the-wire transition in Phase 1 lossless (see §4.2).
- **Two version fields.** `version` keeps its current meaning (author-facing policy-set version, e.g. `"1.0"`). `manifest_schema_version` is new and describes the *wire contract* so a consumer can reject manifests newer than it understands. Conflating the two today is a latent bug; the proto era is a good time to split them.
- **Maps.** `attrs`, `identity_claims`, `expanded_from`, and `extra` become proto `map`s. Maps are convenient but unordered on the wire — which is exactly why signing must not depend on re-serializing them (see §4.4).

---

## Part 4: Wire-compat and migration strategy

The golden rule: **never have a flag day.** Engines and the resolver deploy independently, and old signed bundles/manifests must keep verifying during the rollout.

### 4.1 Versioning

- `manifest_schema_version` lets a consumer say "I understand v1; reject v2." Start at `1` (absent/`0` ⇒ treat as v1 for backfill).
- Proto field numbers are the durable contract. **Never reuse a number; `reserved` removed ones.** This is enforceable with `buf breaking` in CI.
- The author-facing `version` string is untouched and keeps flowing through.

### 4.2 JSON ⇄ proto via the proto3 JSON mapping

Protobuf defines a **canonical JSON mapping**. Generated types can both *emit* and *parse* JSON, with options to **preserve original (snake_case) field names** and to **emit default/absent fields** the way serde does today. Because the draft keeps the current field names and optionality, a proto-generated JSON encoder can produce JSON that the current serde/Gson readers still parse, and vice versa.

This is the crux of a safe migration: **adopt the schema and the generated types without changing a single byte on the wire first.** The win is structural (no more hand-drift, real enums) before any format change risk is taken on.

Caveat: proto3 JSON renders enums as their **names** (`PERMIT`) by default, whereas today's JSON uses lowercase `permit` / `row_filter`. Two clean options: (a) keep enum *values* as the existing lowercase strings via a small custom (de)serializer in the generated-type wrapper, or (b) accept a one-time content change and dual-read both spellings during transition. Recommendation: (a) for zero on-disk churn.

### 4.3 Supporting both formats during transition

- **Files:** keep `manifest.json`; add an optional `manifest.pb` (binary). `FileManifestStore` sniffs by extension/magic and decodes accordingly. Both round-trip through the same generated type.
- **HTTP:** content-negotiate. The sidecar serves `application/json` (default, debuggable) and `application/x-protobuf` when the client sends `Accept: application/x-protobuf`. The reqwest client opts in behind a config flag. No endpoint change, no flag day.
- **Cache:** the Redis layer can store binary protobuf instead of JSON strings transparently; bump the key prefix (`policast:v2:`) so old and new entries don't collide.

### 4.4 Signing: the one genuinely hard part

Today's HMAC is computed over **canonical JSON** (`serde_json::to_vec` with `signature` zeroed, `BTreeMap` for ordering). Protobuf has **no guaranteed canonical serialization across languages** — map field order and unknown-field handling can differ between prost and protobuf-java, so "re-serialize then HMAC" is unsafe across the Rust resolver and the JVM verifier.

**Recommended fix: sign an envelope over the exact bytes.** The resolver serializes `ResolveBundle` once, puts those literal bytes in `SignedResolveBundle.bundle_bytes`, and HMACs *those bytes*. The verifier MACs `bundle_bytes` as received, then decodes. No party ever needs to reproduce a canonical encoding, and the existing `hmac-sha256:<hex>` tag scheme carries over unchanged. This also removes the current "clone, zero the signature field, re-serialize" dance entirely.

(Alternative considered: a deterministic-serialization profile — sorted map keys, no unknown fields. Rejected for v1 because it pushes a cross-language canonicalization invariant onto every implementer, which is precisely the kind of fragile coupling we are trying to remove.)

---

## Part 5: The cross-language story

The entire point is that **both engines decode the same bytes** from one schema.

- **Rust (`policast-core`, `policast-datafusion`, `policast-uc`):** `prost` for message types (`prost-build` in a `build.rs`, or pre-generated checked-in code to avoid a `protoc` build dependency). For the service, `tonic` is the mature gRPC stack. The Axum sidecar can keep serving JSON via the proto3 JSON mapping while also speaking binary — tonic and axum can co-exist, or the sidecar can move to a Connect-style handler (see §6).
- **Scala/JVM (`policast-spark`):** `ScalaPB` generates idiomatic Scala case classes (a near drop-in for the current hand-written ones) and replaces Gson. Alternatively plain `protobuf-java`. Either way the JVM decodes the same wire bytes the Rust side produced — the drift bug becomes structurally impossible.
- **`buf` for the schema lifecycle:** `buf lint`, `buf format`, `buf breaking`, and a `FileDescriptorSet` published as a build artifact. The descriptor set is also what a Spark-SQL `from_protobuf` path would consume if we ever wanted to land manifests as a DataFrame column (not needed for the plugin, which decodes in JVM code, but worth noting the option exists).

Codegen tooling is intentionally *not* part of this RFC's required work — the draft `.proto` is the artifact; wiring prost/ScalaPB is Phase 1.

---

## Part 6: gRPC / connectRPC — in scope now?

The resolver is the only network hop, and it already works as JSON-over-HTTP. So the question is narrowly: *do we add a `service` definition and adopt gRPC or Connect for it?*

- **gRPC (tonic + grpc-java):** efficient, streaming-capable, mature on both sides. Cost: HTTP/2 everywhere, harder to `curl`, gRPC-Web needed for browsers.
- **connectRPC:** the author's background tool. Connect serves **the same handlers over gRPC, gRPC-Web, and a plain HTTP/1.1 + JSON protocol** simultaneously. That is uniquely attractive here because it **preserves the current `curl`/JSON debuggability** while adding binary and generated typed clients — the sidecar could expose the *exact* `POST /policies/resolve` JSON behavior and a binary path from one definition. Connect is strongest on Go/TS/Kotlin/Swift; Rust support is younger (gRPC via tonic is the safe Rust default today), so a pragmatic split is **tonic on the Rust side, Connect semantics at the edge**.

**Recommendation: not now.** Ship the message schema and binary manifest first (Phases 1–2). The structural wins (no drift, enum guards, breaking-change CI, smaller payloads) are independent of the transport and carry ~90% of the value. Add the `PolicyResolver` service in Phase 3, and prefer **connectRPC at the sidecar edge** specifically to keep JSON/curl debuggability while gaining binary + codegen clients. The draft already includes the `service` so reviewers can react to it, but building it is explicitly deferred.

---

## Part 7: Trade-off matrix

| Dimension | JSON (today) | Protobuf (proposed) |
|---|---|---|
| **Schema source of truth** | Implicit; two hand-written copies (Rust serde + Scala Gson) | One `.proto`, generated types |
| **Cross-language drift** | Real and present (Spark missing `target_tag`/`applies_to_tag`) | Structurally prevented by codegen |
| **Enums** | Bare strings; unknown → silent `row_filter` | Closed enums + `UNSPECIFIED` guard → fail closed |
| **Breaking-change detection** | None | `buf breaking` in CI |
| **Payload size / parse speed** | Larger, slower (esp. pretty-printed) | Smaller, faster (matters on hot path) |
| **Human readability / debuggability** | Excellent (`curl`, eyeball, diff) | Binary is opaque; regained via proto3 JSON / Connect JSON |
| **On-disk artifacts** | `manifest.json`, readable in PRs | `manifest.pb` opaque; keep JSON for review |
| **Signing** | Canonical-JSON HMAC (works, fragile coupling) | Envelope-over-bytes HMAC (robust, simpler) |
| **CEL expression** | Opaque string | Still an opaque string (no change) |
| **Ecosystem maturity (Rust)** | serde: excellent | prost/tonic: excellent; Connect: younger |
| **Ecosystem maturity (JVM)** | Gson: fine | ScalaPB/protobuf-java: excellent |
| **Migration cost** | n/a | Real but stageable; JSON-on-wire first de-risks it |
| **`protoc`/codegen build dep** | None | New (mitigate with checked-in generated code) |

**Net:** Protobuf clearly wins on *correctness and contract safety* — the things a governance system should care about most — at the cost of human-readability (recoverable) and some build complexity (manageable). The opaque-CEL caveat is a wash.

---

## Part 8: Phased recommendation

**Phase 0 — Adopt the schema as documentation (this RFC).**
Land `proto/policast/v1/policast.proto` as the agreed source of truth. Add `buf lint`/`buf format` (no codegen yet). Zero runtime risk. *(This RFC + the draft proto are Phase 0.)*

**Phase 1 — Generate types, keep JSON on the wire.**
prost (Rust) + ScalaPB (Spark) generate the model. Engines decode via generated types using the **proto3 JSON mapping with preserved snake_case field names** and lowercase-enum compatibility. This **deletes the hand-maintained Scala case classes** and **fixes the `target_tag`/`applies_to_tag` drift structurally**. No byte changes on disk or the wire. Highest value-to-risk ratio.

**Phase 2 — Binary as an option, and fix signing.**
Add `application/x-protobuf` content negotiation to the sidecar/client, an optional `manifest.pb` file format, and a `policast:v2:` Redis namespace. Move signing to `SignedResolveBundle` (**envelope over exact bytes**). Dual-read everywhere; binary is opt-in.

**Phase 3 — Service contract (gRPC/connectRPC).**
Introduce `service PolicyResolver`. Prefer **connectRPC at the edge** to keep JSON/curl debuggability while gaining binary + generated clients; tonic for native Rust gRPC. Turn on `buf breaking` in CI as the permanent guardrail.

**Recommended stopping point for issue #9:** approve Phases 0–1 as the committed plan, scope Phase 2 as a fast-follow, and keep Phase 3 as "later, probably Connect." The single highest-leverage outcome is **one schema + generated types** — it pays for itself by making the Spark drift bug impossible — and it can ship without changing a single byte on the wire.

---

## Key trade-offs the reader should weigh

1. **Debuggability vs. safety.** Are we comfortable trading eyeball-able JSON for binary, given we can claw most of it back with proto3 JSON / Connect JSON for dev and review?
2. **Build complexity.** Accept a `protoc`/buf step (mitigated by checked-in generated code), or keep serde/Gson hand-sync forever?
3. **Signing model change.** The envelope-over-bytes approach is the clean answer to proto's non-canonical serialization, but it changes how `ResolveBundle` is signed and verified — a deliberate, security-sensitive change to coordinate.
4. **How far to go.** The structural wins (schema, enums, breaking-change CI) are independent of gRPC/Connect. We can capture them via Phase 1 alone and decide on transport later.

## Appendix: file/symbol index (where the work lands)

| Concern | File(s) |
|---|---|
| Canonical manifest types | `crates/policast-core/src/model.rs`, `crates/policast-core/src/policy_manifest.rs` |
| File load + cache + (de)serialize | `crates/policast-core/src/policy_store.rs` |
| Resolver wire types | `crates/policast-uc/src/types.rs` |
| HTTP client (JSON) | `crates/policast-uc/src/client.rs` |
| Sidecar server (JSON) | `crates/policast-uc/src/sidecar.rs` |
| HMAC signing (canonical JSON) | `crates/policast-uc/src/signature.rs` |
| Spark parallel model (Gson) | `policast-spark/src/main/scala/com/policast/spark/PolicyManifest.scala` |
| On-disk example artifact | `examples/policies/manifest.json` |
| Draft schema (this RFC) | `proto/policast/v1/policast.proto` |

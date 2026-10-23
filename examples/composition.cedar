// Alternative grants: regional analysts OR the treating physician.
@id("regional_grant")
@filter_type("row_grant")
@target_table("patients")
@roles("analyst")
permit(principal, action, resource)
when { resource.region == principal.region };

@id("physician_grant")
@filter_type("row_grant")
@target_table("patients")
@roles("physician")
permit(principal, action, resource)
when { resource.treating_physician == principal.name };

// Mandatory restriction applies regardless of which grant admits the row.
@id("active_rows_only")
@filter_type("row_filter")
@target_table("patients")
permit(principal, action, resource)
when { resource.active == true };

@id("legal_hold")
@filter_type("deny_override")
@target_table("patients")
forbid(principal, action, resource)
when { resource.legal_hold == true }
unless { principal.role == "legal" };

@id("ssn_mask")
@filter_type("column_mask")
@target_table("patients")
@column("ssn")
forbid(principal, action, resource)
unless { principal.role == "admin" };

//! MDB introspection: parse `src/mdb/meta.rs` and `src/mdb/optionlist.rs`
//! (serde structs) into an [`MdbModel`] (template_vars filled separately).

use crate::model::{MdbField, MdbModel, MdbTable};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use syn::{
    parse_file, Attribute, Expr, ExprLit, Field, Fields, GenericArgument, Lit, PathArguments, Type,
    TypePath,
};

/// Parse the MDB serde structs. Leaves `template_vars` empty (filled by
/// [`super::template_vars`]).
pub fn introspect(repo_root: &Path) -> Result<MdbModel> {
    let meta_path = repo_root.join("src").join("mdb").join("meta.rs");
    let meta_source = std::fs::read_to_string(&meta_path)?;
    let meta_file = parse_file(&meta_source)?;

    let optionlist_path = repo_root.join("src").join("mdb").join("optionlist.rs");
    let optionlist_source = std::fs::read_to_string(&optionlist_path)?;
    let optionlist_file = parse_file(&optionlist_source)?;

    // Collect all structs by name from meta.rs
    let mut meta_structs: HashMap<String, &syn::ItemStruct> = HashMap::new();
    for item in &meta_file.items {
        if let syn::Item::Struct(s) = item {
            meta_structs.insert(s.ident.to_string(), s);
        }
    }

    // Find public Deserialize structs in meta.rs
    let mut meta_tables = Vec::new();
    let mut struct_order = Vec::new();

    // Process Meta first (top-level struct)
    if let Some(meta_struct) = meta_structs.get("Meta") {
        let table = process_meta_struct(meta_struct, &meta_structs)?;
        meta_tables.push(table);
        struct_order.push("Meta");

        // Extract field order from Meta to determine processing order
        if let Fields::Named(fields) = &meta_struct.fields {
            for field in &fields.named {
                let field_type = type_name(&field.ty);

                // Map field types to struct names we should process
                match field_type.as_str() {
                    "MachineInfo" => struct_order.push("MachineInfo"),
                    "Paths" => struct_order.push("Paths"),
                    "Hardware" => struct_order.push("Hardware"),
                    "Build" => struct_order.push("Build"),
                    "Environment" => struct_order.push("Environment"),
                    "Scheduler" => struct_order.push("Scheduler"),
                    "IndexMap < String , Queue >" => struct_order.push("Queue"),
                    "Variants" => struct_order.push("Variants"),
                    "IndexMap < String , Universe >" => struct_order.push("Universe"),
                    "CactupMeta" => struct_order.push("CactupMeta"),
                    _ => {}
                }
            }
        }
    }

    // Process remaining structs in order
    let mut processed = std::collections::HashSet::new();
    processed.insert("Meta");

    for struct_name in struct_order
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>()
    {
        if processed.contains(struct_name) {
            continue;
        }
        if let Some(s) = meta_structs.get(struct_name) {
            let toml_path = map_struct_to_toml_path(struct_name);
            let table = process_struct_to_table(s, struct_name, toml_path)?;
            meta_tables.push(table);
            processed.insert(struct_name);
        }
    }

    // Process any remaining structs not in the dependency chain
    for (name, _) in meta_structs.iter() {
        if !processed.contains(name.as_str())
            && let Some(s) = meta_structs.get(name)
            && is_public_deserialize(s)
        {
            let toml_path = map_struct_to_toml_path(name.as_str());
            let table = process_struct_to_table(s, name, toml_path)?;
            meta_tables.push(table);
        }
    }

    // Find OptionlistHeader in optionlist.rs
    let mut optionlist_header = MdbTable::default();
    for item in &optionlist_file.items {
        if let syn::Item::Struct(s) = item
            && s.ident == "OptionlistHeader" && is_public_deserialize(s)
        {
            optionlist_header =
                process_struct_to_table(s, "OptionlistHeader", Some("[cactup]"))?;
        }
    }

    Ok(MdbModel {
        meta_tables,
        optionlist_header,
        template_vars: Vec::new(),
    })
}

/// Map a struct name to its TOML path (for meta.rs structs)
fn map_struct_to_toml_path(struct_name: &str) -> Option<&'static str> {
    match struct_name {
        "MachineInfo" => Some("[machine]"),
        "Paths" => Some("[paths]"),
        "Hardware" => Some("[hardware]"),
        "Build" => Some("[build]"),
        "Environment" => Some("[environment]"),
        "Scheduler" => Some("[scheduler]"),
        "Queue" => Some("[queues.<name>]"),
        "Variants" => Some("[variants]"),
        "OptionlistVariants" => Some("[variants.optionlist]"),
        "Universe" => Some("[universes.<name>]"),
        "CactupMeta" => Some("[cactup]"),
        _ => None,
    }
}

/// Process the Meta struct specially to extract toml_path mappings for each field type
fn process_meta_struct(
    meta_struct: &syn::ItemStruct,
    _all_structs: &HashMap<String, &syn::ItemStruct>,
) -> Result<MdbTable> {
    let mut table = MdbTable {
        struct_name: "Meta".to_string(),
        toml_path: None,
        doc: extract_doc(&meta_struct.attrs).0,
        fields: Vec::new(),
    };

    if let Fields::Named(fields) = &meta_struct.fields {
        for field in &fields.named {
            let mdb_field = field_to_mdb_field(field, meta_struct)?;
            table.fields.push(mdb_field);
        }
    }

    Ok(table)
}

/// Process a struct to create an MdbTable
fn process_struct_to_table(
    s: &syn::ItemStruct,
    struct_name: &str,
    toml_path: Option<&str>,
) -> Result<MdbTable> {
    let mut table = MdbTable {
        struct_name: struct_name.to_string(),
        toml_path: toml_path.map(|p| p.to_string()),
        doc: extract_doc(&s.attrs).0,
        fields: Vec::new(),
    };

    if let Fields::Named(fields) = &s.fields {
        for field in &fields.named {
            let mdb_field = field_to_mdb_field(field, s)?;
            table.fields.push(mdb_field);
        }
    }

    Ok(table)
}

/// Convert a syn::Field to an MdbField
fn field_to_mdb_field(field: &Field, parent_struct: &syn::ItemStruct) -> Result<MdbField> {
    let rust_field = field.ident.as_ref().unwrap().to_string();

    // Determine toml_key from serde attributes
    let toml_key = extract_toml_key(field, parent_struct)?;

    // Extract type description
    let type_desc = describe_type(&field.ty);

    // Check if optional
    let optional = is_optional_type(&field.ty) || has_serde_default(field);

    // Extract default note
    let default_note = optional.then(|| "optional".to_string());

    // Extract doc comment
    let (doc, _) = extract_doc(&field.attrs);

    Ok(MdbField {
        toml_key,
        rust_field,
        type_desc,
        optional,
        default_note,
        doc,
    })
}

/// Extract the toml key for a field (considering serde attributes and rename_all)
fn extract_toml_key(field: &Field, parent_struct: &syn::ItemStruct) -> Result<String> {
    let field_name = field.ident.as_ref().unwrap().to_string();

    // Check for #[serde(rename = "...")]
    for attr in &field.attrs {
        if let syn::Meta::List(list) = &attr.meta
            && list.path.is_ident("serde")
            && let Ok(syn::Meta::NameValue(nv)) = list.parse_args::<syn::Meta>()
            && nv.path.is_ident("rename")
            && let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(lit_str), .. }) = &nv.value
        {
            return Ok(lit_str.value());
        }
    }

    // Check parent struct for #[serde(rename_all = "kebab-case")]
    let mut rename_all_kebab = false;
    for attr in &parent_struct.attrs {
        if let syn::Meta::List(list) = &attr.meta
            && list.path.is_ident("serde")
            && let Ok(syn::Meta::NameValue(nv)) = list.parse_args::<syn::Meta>()
            && nv.path.is_ident("rename_all")
            && let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(lit_str), .. }) = &nv.value
            && lit_str.value() == "kebab-case"
        {
            rename_all_kebab = true;
        }
    }

    if rename_all_kebab {
        Ok(to_kebab_case(&field_name))
    } else {
        Ok(field_name)
    }
}

/// Convert snake_case to kebab-case
fn to_kebab_case(s: &str) -> String {
    s.replace('_', "-")
}

/// Check if a field is Option<T>
fn is_optional_type(ty: &Type) -> bool {
    if let Type::Path(TypePath { path, .. }) = ty && let Some(segment) = path.segments.last() {
        return segment.ident == "Option";
    }
    false
}

/// Check if field has #[serde(default)] or #[serde(default = "...")]
fn has_serde_default(field: &Field) -> bool {
    for attr in &field.attrs {
        if let syn::Meta::List(list) = &attr.meta && list.path.is_ident("serde") {
            // Parse as a flat list of metas
            if let Ok(meta) = list.parse_args::<syn::MetaNameValue>() {
                if meta.path.is_ident("default") {
                    return true;
                }
            } else if let Ok(meta) = list.parse_args::<syn::Ident>() && meta == "default" {
                return true;
            }
        }
    }
    false
}

/// Get the inner type of Option<T>
fn get_option_inner_type(ty: &Type) -> Option<String> {
    if let Type::Path(TypePath { path, .. }) = ty
        && let Some(segment) = path.segments.last()
        && segment.ident == "Option"
        && let PathArguments::AngleBracketed(args) = &segment.arguments
        && let Some(GenericArgument::Type(inner)) = args.args.first()
    {
        return Some(describe_type(inner));
    }
    None
}

/// Extract type name from a Type for comparison
fn type_name(ty: &Type) -> String {
    format_type_name(ty)
}

/// Format a type for comparison purposes
fn format_type_name(ty: &Type) -> String {
    match ty {
        Type::Path(TypePath { path, .. }) => {
            let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();

            // Special handling for generic types
            if let Some(segment) = path.segments.last()
                && (segment.ident == "Option"
                    || segment.ident == "Vec"
                    || segment.ident == "IndexMap")
            {
                let mut result = segment.ident.to_string();
                if let PathArguments::AngleBracketed(args) = &segment.arguments {
                    result.push_str(" < ");
                    let arg_strs: Vec<String> = args
                        .args
                        .iter()
                        .map(|arg| match arg {
                            GenericArgument::Type(t) => format_type_name(t),
                            GenericArgument::Const(c) => quote_const(c).to_string(),
                            _ => "?".to_string(),
                        })
                        .collect();
                    result.push_str(&arg_strs.join(" , "));
                    result.push_str(" >");
                }
                return result;
            }

            // For non-generic types, just use the last segment
            segments
                .last()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        }
        _ => "unknown".to_string(),
    }
}

/// Helper to quote a const (simplified)
fn quote_const(c: &Expr) -> String {
    match c {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => format!("\"{}\"", s.value()),
        Expr::Lit(ExprLit {
            lit: Lit::Int(i), ..
        }) => i.base10_digits().to_string(),
        _ => "?".to_string(),
    }
}

/// Generate human-readable description of a type
fn describe_type(ty: &Type) -> String {
    match ty {
        Type::Path(TypePath { path, .. }) => {
            let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();

            // Special case for Option<T>
            if segments.len() == 1 && segments[0] == "Option"
                && let Some(inner) = get_option_inner_type(ty)
            {
                return inner;
            }

            // Special case for Vec<T>
            if segments.len() == 1 && segments[0] == "Vec"
                && let Type::Path(TypePath { path, .. }) = ty
                && let Some(segment) = path.segments.last()
                && let PathArguments::AngleBracketed(args) = &segment.arguments
                && let Some(GenericArgument::Type(inner)) = args.args.first()
            {
                let inner_desc = describe_type(inner);
                return format!("list of {}", inner_desc);
            }

            // Special case for IndexMap<K, V>
            if segments.last().map(|s| s.as_str()) == Some("IndexMap")
                && let Type::Path(TypePath { path, .. }) = ty
                && let Some(segment) = path.segments.last()
                && let PathArguments::AngleBracketed(args) = &segment.arguments
            {
                let mut type_args = args.args.iter().filter_map(|arg| {
                    if let GenericArgument::Type(t) = arg {
                        Some(describe_type(t))
                    } else {
                        None
                    }
                });
                if let Some(_key_type) = type_args.next()
                    && let Some(value_type) = type_args.next()
                {
                    return format!("table of {}", value_type);
                }
            }

            // Handle primitives
            match segments.last().map(|s| s.as_str()) {
                Some("String") => "string".to_string(),
                Some("bool") => "boolean".to_string(),
                Some("u32") | Some("u64") | Some("i32") | Some("i64") => "integer".to_string(),
                Some("Walltime") => "walltime (DD-HH:MM:SS)".to_string(),
                Some("PathBuf") => "path".to_string(),
                Some(name) => format!("table ({})", name),
                None => "unknown".to_string(),
            }
        }
        _ => "unknown".to_string(),
    }
}

/// Extract doc comments from attributes
fn extract_doc(attrs: &[Attribute]) -> (Option<String>, Option<String>) {
    let mut doc_lines = Vec::new();

    for attr in attrs {
        if let syn::Meta::NameValue(nv) = &attr.meta
            && nv.path.is_ident("doc")
            && let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(lit_str), .. }) = &nv.value
        {
            let line = lit_str.value();
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                doc_lines.push(trimmed.to_string());
            }
        }
    }

    if doc_lines.is_empty() {
        return (None, None);
    }

    let combined = doc_lines.join(" ");
    (Some(combined), None)
}

/// Check if a struct is public and has #[derive(Deserialize)]
fn is_public_deserialize(s: &syn::ItemStruct) -> bool {
    matches!(s.vis, syn::Visibility::Public(_)) && has_deserialize_derive(&s.attrs)
}

/// Check if a struct has #[derive(Deserialize)]
fn has_deserialize_derive(attrs: &[Attribute]) -> bool {
    for attr in attrs {
        if let syn::Meta::List(list) = &attr.meta && list.path.is_ident("derive") {
            // Try parsing the derive contents
            let tokens = &list.tokens;
            let s = tokens.to_string();
            if s.contains("Deserialize") {
                return true;
            }
        }
    }
    false
}

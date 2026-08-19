//! CLI introspection: parse `src/args.rs` (clap derive) into a [`CliModel`].

use crate::model::{CliArg, CliCommand, CliModel};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::Path;
use syn::{
    parse_file, Attribute, ExprLit, Field, Fields, GenericArgument, Lit, Meta,
    PathArguments, Type, TypePath, Variant,
};

/// Parse `<repo_root>/src/args.rs` and build the command tree.
pub fn introspect(repo_root: &Path) -> Result<CliModel> {
    let args_path = repo_root.join("src").join("args.rs");
    let source = std::fs::read_to_string(&args_path)?;
    let file = parse_file(&source)?;

    // Collect all structs and enums by name for resolution
    let mut structs: HashMap<String, &syn::ItemStruct> = HashMap::new();
    let mut enums: HashMap<String, &syn::ItemEnum> = HashMap::new();

    for item in &file.items {
        match item {
            syn::Item::Struct(s) => {
                structs.insert(s.ident.to_string(), s);
            }
            syn::Item::Enum(e) => {
                enums.insert(e.ident.to_string(), e);
            }
            _ => {}
        }
    }

    // Find the root command (struct with #[derive(Parser)])
    let root_struct = structs
        .values()
        .find(|s| has_derive_parser(&s.attrs))
        .ok_or_else(|| anyhow!("No struct with #[derive(Parser)] found in args.rs"))?;

    let root_command = process_struct(root_struct, "cactup", vec!["cactup".to_string()], &structs, &enums)?;

    Ok(CliModel { root: root_command })
}

fn has_derive_parser(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if let Meta::List(list) = &attr.meta
            && list.path.is_ident("derive")
            && let Ok(metas) = parse_clap_metas(list)
        {
            return metas.iter().any(|m| {
                if let Meta::Path(p) = m {
                    p.is_ident("Parser")
                } else {
                    false
                }
            });
        }
        false
    })
}

fn process_struct(
    s: &syn::ItemStruct,
    name: &str,
    path: Vec<String>,
    structs: &HashMap<String, &syn::ItemStruct>,
    enums: &HashMap<String, &syn::ItemEnum>,
) -> Result<CliCommand> {
    let (about, long_about) = extract_doc(&s.attrs);

    let mut args = Vec::new();
    let mut subcommands = Vec::new();

    if let Fields::Named(fields) = &s.fields {
        for field in &fields.named {
            if let Some(flatten_struct_name) = get_flatten_attr(field) {
                if let Some(flatten_struct) = structs.get(&flatten_struct_name) {
                    let flattened = expand_flattened_struct(flatten_struct, structs)?;
                    args.extend(flattened);
                }
            } else if is_subcommand_field(field) {
                let type_name = extract_type_name(&field.ty)?;
                if let Some(enum_def) = enums.get(&type_name) {
                    for variant in &enum_def.variants {
                        let subcommand = process_subcommand_variant(
                            variant,
                            &path,
                            structs,
                            enums,
                        )?;
                        subcommands.push(subcommand);
                    }
                }
            } else {
                if let Some(arg) = field_to_arg(field)? {
                    args.push(arg);
                }
            }
        }
    }

    Ok(CliCommand {
        name: name.to_string(),
        path,
        about,
        long_about,
        args,
        subcommands,
    })
}

// No `enums` parameter: subcommands inside a flattened struct are not
// expanded here, so nothing in this recursion ever needs the enum table.
fn expand_flattened_struct(
    s: &syn::ItemStruct,
    structs: &HashMap<String, &syn::ItemStruct>,
) -> Result<Vec<CliArg>> {
    let mut args = Vec::new();

    if let Fields::Named(fields) = &s.fields {
        for field in &fields.named {
            if let Some(flatten_struct_name) = get_flatten_attr(field) {
                if let Some(flatten_struct) = structs.get(&flatten_struct_name) {
                    let flattened = expand_flattened_struct(flatten_struct, structs)?;
                    args.extend(flattened);
                }
            } else if is_subcommand_field(field) {
                // Subcommands in flattened structs are not expanded here
            } else {
                if let Some(arg) = field_to_arg(field)? {
                    args.push(arg);
                }
            }
        }
    }

    Ok(args)
}

fn process_subcommand_variant(
    variant: &Variant,
    parent_path: &[String],
    structs: &HashMap<String, &syn::ItemStruct>,
    enums: &HashMap<String, &syn::ItemEnum>,
) -> Result<CliCommand> {
    let variant_name = to_kebab_case(&variant.ident.to_string());
    let (about, long_about) = extract_doc(&variant.attrs);

    let mut path = parent_path.to_vec();
    path.push(variant_name.clone());

    match &variant.fields {
        Fields::Unit => {
            Ok(CliCommand {
                name: variant_name,
                path,
                about,
                long_about,
                args: vec![],
                subcommands: vec![],
            })
        }
        Fields::Named(fields) => {
            let mut args = Vec::new();
            let mut subcommands = Vec::new();

            for field in &fields.named {
                if let Some(flatten_struct_name) = get_flatten_attr(field) {
                    if let Some(flatten_struct) = structs.get(&flatten_struct_name) {
                        let flattened = expand_flattened_struct(flatten_struct, structs)?;
                        args.extend(flattened);
                    }
                } else if is_subcommand_field(field) {
                    let type_name = extract_type_name(&field.ty)?;
                    if let Some(enum_def) = enums.get(&type_name) {
                        for sub_variant in &enum_def.variants {
                            let subcommand =
                                process_subcommand_variant(sub_variant, &path, structs, enums)?;
                            subcommands.push(subcommand);
                        }
                    }
                } else {
                    if let Some(arg) = field_to_arg(field)? {
                        args.push(arg);
                    }
                }
            }

            Ok(CliCommand {
                name: variant_name,
                path,
                about,
                long_about,
                args,
                subcommands,
            })
        }
        Fields::Unnamed(fields) => {
            if fields.unnamed.len() == 1 {
                let field_type = &fields.unnamed[0].ty;
                let type_name = extract_type_name(field_type)?;

                // Check if it's an Args struct or a Subcommand enum
                if let Some(args_struct) = structs.get(&type_name) {
                    // It's an Args struct - expand its fields
                    let mut args = Vec::new();
                    let mut subcommands = Vec::new();

                    if let Fields::Named(struct_fields) = &args_struct.fields {
                        for struct_field in &struct_fields.named {
                            if let Some(flatten_struct_name) = get_flatten_attr(struct_field) {
                                if let Some(flatten_struct) = structs.get(&flatten_struct_name) {
                                    let flattened =
                                        expand_flattened_struct(flatten_struct, structs)?;
                                    args.extend(flattened);
                                }
                            } else if is_subcommand_field(struct_field) {
                                let sub_type_name = extract_type_name(&struct_field.ty)?;
                                if let Some(sub_enum) = enums.get(&sub_type_name) {
                                    for sub_variant in &sub_enum.variants {
                                        let subcommand = process_subcommand_variant(
                                            sub_variant,
                                            &path,
                                            structs,
                                            enums,
                                        )?;
                                        subcommands.push(subcommand);
                                    }
                                }
                            } else {
                                if let Some(arg) = field_to_arg(struct_field)? {
                                    args.push(arg);
                                }
                            }
                        }
                    }

                    Ok(CliCommand {
                        name: variant_name,
                        path,
                        about,
                        long_about,
                        args,
                        subcommands,
                    })
                } else if let Some(subcommand_enum) = enums.get(&type_name) {
                    // It's a Subcommand enum - expand its variants as nested subcommands
                    let mut subcommands = Vec::new();
                    for sub_variant in &subcommand_enum.variants {
                        let subcommand =
                            process_subcommand_variant(sub_variant, &path, structs, enums)?;
                        subcommands.push(subcommand);
                    }

                    Ok(CliCommand {
                        name: variant_name,
                        path,
                        about,
                        long_about,
                        args: vec![],
                        subcommands,
                    })
                } else {
                    Err(anyhow!("Unknown type for newtype variant: {}", type_name))
                }
            } else {
                Err(anyhow!("Unsupported: multiple unnamed fields in variant"))
            }
        }
    }
}

fn get_flatten_attr(field: &Field) -> Option<String> {
    for attr in &field.attrs {
        if let Meta::List(list) = &attr.meta
            && list.path.is_ident("clap")
            && let Ok(metas) = parse_clap_metas(list)
        {
            for meta in metas {
                if let Meta::Path(p) = meta && p.is_ident("flatten") {
                    // Return the type name to signal that this is a flatten field
                    return extract_type_name(&field.ty).ok();
                }
            }
        }
    }
    None
}

fn is_subcommand_field(field: &Field) -> bool {
    for attr in &field.attrs {
        if let Meta::List(list) = &attr.meta
            && list.path.is_ident("clap")
            && let Ok(metas) = parse_clap_metas(list)
        {
            for meta in metas {
                if let Meta::Path(p) = meta && p.is_ident("subcommand") {
                    return true;
                }
            }
        }
    }
    false
}


fn extract_type_name(ty: &Type) -> Result<String> {
    if let Type::Path(TypePath { path, .. }) = ty && let Some(segment) = path.segments.last() {
        return Ok(segment.ident.to_string());
    }
    Err(anyhow!("Cannot extract type name"))
}

fn field_to_arg(field: &Field) -> Result<Option<CliArg>> {
    if get_flatten_attr(field).is_some() || is_subcommand_field(field) {
        return Ok(None);
    }

    let field_name = field
        .ident
        .as_ref()
        .ok_or_else(|| anyhow!("Unnamed field"))?
        .to_string();

    let (short, long) = extract_short_long(&field_name, &field.attrs)?;
    let (takes_value, required, multiple) = analyze_type(&field.ty);
    let default = extract_default_value(&field.attrs);
    let global = is_global(&field.attrs);
    let positional = short.is_none() && long.is_none();
    let value_name = extract_value_name(&field.attrs);
    let (help, long_help) = extract_help(&field.attrs, &extract_doc(&field.attrs));

    let required = required && !positional && default.is_none();

    Ok(Some(CliArg {
        name: field_name,
        short,
        long,
        value_name,
        positional,
        required,
        takes_value,
        multiple,
        default,
        global,
        help,
        long_help,
    }))
}

fn parse_clap_metas(list: &syn::MetaList) -> Result<Vec<Meta>> {
    let tokens = list.tokens.to_string();
    let mut metas = Vec::new();

    // Simple split on commas to get individual items, then parse each
    for item in tokens.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }

        // Try to parse as a Meta using syn::parse_str
        if let Ok(meta) = syn::parse_str::<Meta>(item) {
            metas.push(meta);
        }
    }

    Ok(metas)
}

fn extract_short_long(field_name: &str, attrs: &[Attribute]) -> Result<(Option<char>, Option<String>)> {
    let mut short = None;
    let mut long = None;

    for attr in attrs {
        if let Meta::List(list) = &attr.meta {
            if !list.path.is_ident("clap") {
                continue;
            }

            // Try to parse all Meta items in the clap attribute
            if let Ok(metas) = parse_clap_metas(list) {
                for meta in metas {
                    if let Meta::NameValue(nv) = meta {
                        if nv.path.is_ident("short") {
                            if let syn::Expr::Lit(ExprLit {
                                lit: Lit::Char(lit_char),
                                ..
                            }) = &nv.value
                            {
                                short = Some(lit_char.value());
                            }
                        } else if nv.path.is_ident("long")
                            && let syn::Expr::Lit(ExprLit { lit: Lit::Str(lit_str), .. }) =
                                &nv.value
                        {
                            long = Some(lit_str.value());
                        }
                    }
                }
            }
        }
    }

    // If no long was found, default to kebab-case of field name
    if long.is_none() {
        long = Some(to_kebab_case(field_name));
    }

    // If no short was found but we have a long, default to first char
    if short.is_none() && long.is_some()
        && let Some(first_char) = long.as_ref().and_then(|s| s.chars().next())
    {
        short = Some(first_char);
    }

    Ok((short, long))
}

fn analyze_type(ty: &Type) -> (bool, bool, bool) {
    match ty {
        Type::Path(TypePath { path, .. }) => {
            if let Some(segment) = path.segments.last() {
                let ident = &segment.ident;

                if ident == "bool" {
                    return (false, false, false);
                }

                if ident == "Option" {
                    if let PathArguments::AngleBracketed(args) = &segment.arguments
                        && let Some(GenericArgument::Type(inner_ty)) = args.args.first()
                    {
                        let (inner_takes, _, _) = analyze_type(inner_ty);
                        return (inner_takes || ident == "Option", false, false);
                    }
                    return (true, false, false);
                }

                if ident == "Vec" {
                    return (true, false, true);
                }

                // Default for other types
                return (true, true, false);
            }
        }
        _ => {
            // Default for non-path types
            return (true, true, false);
        }
    }
    (true, true, false)
}

fn extract_default_value(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if let Meta::List(list) = &attr.meta
            && list.path.is_ident("clap")
            && let Ok(metas) = parse_clap_metas(list)
        {
            for meta in metas {
                if let Meta::NameValue(nv) = meta
                    && nv.path.is_ident("default_value")
                    && let syn::Expr::Lit(ExprLit { lit: Lit::Str(lit_str), .. }) = &nv.value
                {
                    return Some(lit_str.value());
                }
            }
        }
    }
    None
}

fn is_global(attrs: &[Attribute]) -> bool {
    for attr in attrs {
        if let Meta::List(list) = &attr.meta
            && list.path.is_ident("clap")
            && let Ok(metas) = parse_clap_metas(list)
        {
            for meta in metas {
                if let Meta::NameValue(nv) = meta
                    && nv.path.is_ident("global")
                    && let syn::Expr::Lit(ExprLit { lit: Lit::Bool(lit_bool), .. }) = &nv.value
                {
                    return lit_bool.value();
                }
            }
        }
    }
    false
}

fn extract_value_name(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if let Meta::List(list) = &attr.meta
            && list.path.is_ident("clap")
            && let Ok(metas) = parse_clap_metas(list)
        {
            for meta in metas {
                if let Meta::NameValue(nv) = meta
                    && nv.path.is_ident("value_name")
                    && let syn::Expr::Lit(ExprLit { lit: Lit::Str(lit_str), .. }) = &nv.value
                {
                    return Some(lit_str.value());
                }
            }
        }
    }
    None
}

fn extract_help(attrs: &[Attribute], doc: &(Option<String>, Option<String>)) -> (Option<String>, Option<String>) {
    for attr in attrs {
        if let Meta::List(list) = &attr.meta
            && list.path.is_ident("clap")
            && let Ok(metas) = parse_clap_metas(list)
        {
            for meta in metas {
                if let Meta::NameValue(nv) = meta
                    && nv.path.is_ident("help")
                    && let syn::Expr::Lit(ExprLit { lit: Lit::Str(lit_str), .. }) = &nv.value
                {
                    let help = lit_str.value();
                    return (Some(help), doc.1.clone());
                }
            }
        }
    }
    (doc.0.clone(), doc.1.clone())
}

fn extract_doc(attrs: &[Attribute]) -> (Option<String>, Option<String>) {
    let mut doc_lines = Vec::new();

    for attr in attrs {
        if attr.path().is_ident("doc")
            && let Meta::NameValue(nv) = &attr.meta
            && let syn::Expr::Lit(ExprLit { lit: Lit::Str(lit_str), .. }) = &nv.value
        {
            let doc = lit_str.value();
            doc_lines.push(doc);
        }
    }

    if doc_lines.is_empty() {
        return (None, None);
    }

    let full_doc = doc_lines.join("\n");
    let paragraphs: Vec<&str> = full_doc.split("\n\n").map(|s| s.trim()).collect();

    let short = paragraphs.first().map(|p| p.to_string());
    let long = paragraphs.get(1..).and_then(|rest| {
        if rest.is_empty() {
            None
        } else {
            Some(rest.join("\n\n"))
        }
    });

    (short, long)
}

fn to_kebab_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch == '_' {
            result.push('-');
        } else if i > 0 && ch.is_uppercase() {
            result.push('-');
            result.push(ch.to_lowercase().next().unwrap_or(ch));
        } else {
            result.push(ch.to_lowercase().next().unwrap_or(ch));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_kebab_case() {
        assert_eq!(to_kebab_case("mail_type"), "mail-type");
        assert_eq!(to_kebab_case("makeJobs"), "make-jobs");
        assert_eq!(to_kebab_case("simple"), "simple");
    }

    #[test]
    fn test_introspect_parses_args_rs() {
        // This test verifies that the introspect function can parse the actual args.rs file
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir.parent().expect("No parent of manifest dir");

        let result = introspect(repo_root);
        assert!(result.is_ok(), "Failed to introspect: {:?}", result.err());

        let model = result.unwrap();
        assert_eq!(model.root.name, "cactup", "Root command name should be 'cactup'");
        assert!(!model.root.path.is_empty(), "Root path should not be empty");
        assert_eq!(model.root.path[0], "cactup", "Root path should start with 'cactup'");

        // Verify that subcommands exist
        assert!(!model.root.subcommands.is_empty(), "Root should have subcommands");

        // Look for specific subcommands
        let subcommand_names: Vec<_> = model.root.subcommands.iter().map(|c| c.name.as_str()).collect();
        assert!(
            subcommand_names.contains(&"sim"),
            "Expected 'sim' subcommand, found: {:?}",
            subcommand_names
        );
        assert!(
            subcommand_names.contains(&"install"),
            "Expected 'install' subcommand, found: {:?}",
            subcommand_names
        );
        assert!(
            subcommand_names.contains(&"config"),
            "Expected 'config' subcommand, found: {:?}",
            subcommand_names
        );

        // Find the sim command and verify it has subcommands
        let sim_cmd = model
            .root
            .subcommands
            .iter()
            .find(|c| c.name == "sim")
            .expect("sim command not found");
        assert!(
            !sim_cmd.subcommands.is_empty(),
            "sim command should have subcommands"
        );

        let sim_subcommand_names: Vec<_> = sim_cmd.subcommands.iter().map(|c| c.name.as_str()).collect();
        assert!(
            sim_subcommand_names.contains(&"submit"),
            "Expected 'submit' subcommand under sim, found: {:?}",
            sim_subcommand_names
        );
        assert!(
            sim_subcommand_names.contains(&"run"),
            "Expected 'run' subcommand under sim, found: {:?}",
            sim_subcommand_names
        );
    }

    #[test]
    fn test_introspect_global_flags() {
        // Verify that global flags are parsed
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir.parent().expect("No parent of manifest dir");

        let model = introspect(repo_root).expect("Failed to introspect");

        // Root should have args (the global flags)
        assert!(!model.root.args.is_empty(), "Root should have args (global flags)");

        // Look for verbose flag
        let verbose_arg = model
            .root
            .args
            .iter()
            .find(|a| a.name == "verbose")
            .expect("verbose flag not found");
        assert_eq!(verbose_arg.short, Some('v'), "verbose should have short flag 'v'");
        assert_eq!(verbose_arg.long, Some("verbose".to_string()), "verbose should have long flag");
        assert!(!verbose_arg.takes_value, "verbose is a flag, should not take value");
        assert!(verbose_arg.global, "verbose should be a global flag");
    }

    #[test]
    fn test_introspect_topology_flags() {
        // Verify that flattened topology flags appear in sim submit
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir.parent().expect("No parent of manifest dir");

        let model = introspect(repo_root).expect("Failed to introspect");

        let sim_cmd = model
            .root
            .subcommands
            .iter()
            .find(|c| c.name == "sim")
            .expect("sim command not found");

        let submit_cmd = sim_cmd
            .subcommands
            .iter()
            .find(|c| c.name == "submit")
            .expect("submit command not found");

        // Should have flattened topology flags
        let _arg_names: Vec<_> = submit_cmd.args.iter().map(|a| a.name.as_str()).collect();

        // Check for nodes flag (short 'n', long "nodes")
        let nodes_arg = submit_cmd
            .args
            .iter()
            .find(|a| a.name == "nodes")
            .expect("nodes flag not found in sim submit");
        assert_eq!(nodes_arg.short, Some('n'), "nodes should have short flag 'n'");
        assert_eq!(nodes_arg.long, Some("nodes".to_string()), "nodes should have long flag");

        // Check for wall-time flag (short 'w')
        let wall_time_arg = submit_cmd
            .args
            .iter()
            .find(|a| a.name == "wall_time")
            .expect("wall_time flag not found in sim submit");
        assert_eq!(wall_time_arg.short, Some('w'), "wall_time should have short flag 'w'");
        assert_eq!(
            wall_time_arg.long,
            Some("wall-time".to_string()),
            "wall_time should have long flag 'wall-time'"
        );
    }
}

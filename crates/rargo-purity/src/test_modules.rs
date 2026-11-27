use std::collections::BTreeMap;

use quote::ToTokens;
use syn::{Attribute, Item};

pub fn cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        let p = a.path();
        if p.is_ident("test") || p.segments.last().is_some_and(|s| s.ident == "test") {
            return true;
        }
        if !p.is_ident("cfg") {
            return false;
        }
        let text: String = a.meta.to_token_stream().to_string().chars().filter(|c| !c.is_whitespace()).collect();
        text.contains("test") && !text.contains("not(test")
    })
}

pub fn test_files(parsed: &[(&str, syn::File)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (file, parsed) in parsed {
        let dir = match file.rsplit_once('/') {
            Some((d, name)) if name == "lib.rs" || name == "main.rs" || name == "mod.rs" => d.to_string(),
            Some(_) => file.trim_end_matches(".rs").to_string(),
            None => continue,
        };
        for item in &parsed.items {
            if let Item::Mod(m) = item
                && m.content.is_none()
                && cfg_test(&m.attrs)
            {
                for candidate in [format!("{dir}/{}.rs", m.ident), format!("{dir}/{}/mod.rs", m.ident)] {
                    out.insert(candidate, file.to_string());
                }
            }
        }
    }
    out
}

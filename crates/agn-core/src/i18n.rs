//! Translations shared by the web UI and the native parts (tray). The JSON
//! files in `ui/i18n` are the single source of truth.
//!
//! Adding a language: create `ui/i18n/<code>.json` with the same keys, add
//! it to [`LOCALES`] and to the language `<select>` in `ui/index.html`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// `(code, json)` — the first entry is the default language.
pub const LOCALES: &[(&str, &str)] =
    &[("cs", include_str!("../../../ui/i18n/cs.json")), ("en", include_str!("../../../ui/i18n/en.json"))];

const FALLBACK: &str = "en";

fn tables() -> &'static HashMap<&'static str, HashMap<String, String>> {
    static TABLES: OnceLock<HashMap<&'static str, HashMap<String, String>>> = OnceLock::new();
    TABLES.get_or_init(|| {
        LOCALES.iter().map(|(code, raw)| (*code, serde_json::from_str(raw).unwrap_or_default())).collect()
    })
}

pub fn strings(lang: &str) -> HashMap<String, String> {
    tables().get(normalize(lang)).cloned().unwrap_or_default()
}

pub fn t(lang: &str, key: &str) -> String {
    let tables = tables();
    tables
        .get(normalize(lang))
        .and_then(|t| t.get(key))
        .or_else(|| tables.get(FALLBACK).and_then(|t| t.get(key)))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

pub fn normalize(lang: &str) -> &'static str {
    LOCALES.iter().map(|(code, _)| *code).find(|c| *c == lang).unwrap_or(LOCALES[0].0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_locales_parse_and_share_keys() {
        let reference = strings(LOCALES[0].0);
        assert!(reference.len() > 50);
        for (code, _) in LOCALES {
            let table = strings(code);
            let mut missing: Vec<_> = reference.keys().filter(|k| !table.contains_key(*k)).collect();
            missing.extend(table.keys().filter(|k| !reference.contains_key(*k)));
            assert!(missing.is_empty(), "{code}: keys differ: {missing:?}");
        }
        assert_eq!(t("cs", "action.sleep"), "Uspat");
        assert_eq!(t("en", "action.sleep"), "Sleep");
        assert_eq!(t("de", "action.sleep"), "Uspat", "unknown languages fall back to the default");
        assert_eq!(t("en", "nope.key"), "nope.key");
    }
}

//! Translations shared with the web UI (same JSON files), used for the tray.

use std::collections::HashMap;
use std::sync::OnceLock;

const CS: &str = include_str!("../../ui/i18n/cs.json");
const EN: &str = include_str!("../../ui/i18n/en.json");

pub const LANGUAGES: &[&str] = &["cs", "en"];

fn table(lang: &str) -> &'static HashMap<String, String> {
    static CS_T: OnceLock<HashMap<String, String>> = OnceLock::new();
    static EN_T: OnceLock<HashMap<String, String>> = OnceLock::new();
    let parse = |s: &str| serde_json::from_str(s).unwrap_or_default();
    match lang {
        "en" => EN_T.get_or_init(|| parse(EN)),
        _ => CS_T.get_or_init(|| parse(CS)),
    }
}

pub fn t(lang: &str, key: &str) -> String {
    table(lang).get(key).or_else(|| table("en").get(key)).cloned().unwrap_or_else(|| key.to_string())
}

pub fn normalize(lang: &str) -> &'static str {
    LANGUAGES.iter().copied().find(|l| *l == lang).unwrap_or("cs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_parse_and_have_same_keys() {
        let cs = table("cs");
        let en = table("en");
        assert!(cs.len() > 50);
        let mut missing: Vec<_> = cs.keys().filter(|k| !en.contains_key(*k)).collect();
        missing.extend(en.keys().filter(|k| !cs.contains_key(*k)));
        assert!(missing.is_empty(), "keys missing in a language: {missing:?}");
        assert_eq!(t("cs", "action.sleep"), "Uspat");
        assert_eq!(t("de", "action.sleep"), "Uspat", "unknown languages fall back to Czech");
        assert_eq!(t("en", "nope.key"), "nope.key");
    }
}

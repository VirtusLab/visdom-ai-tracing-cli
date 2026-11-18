//! `VISDOM_*` names for the environment variables the tracing library reads
//! as `TRACEVAULT_*`.

/// The variables that have a `VISDOM_` spelling.
pub const ALIASED: [&str; 4] = ["SERVER_URL", "API_KEY", "SESSION_ID", "NO_BROWSER"];

/// `("TRACEVAULT_X", value)` for every `VISDOM_X` that `get` returns.
pub fn alias_pairs(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    ALIASED
        .iter()
        .filter_map(|name| {
            get(&format!("VISDOM_{name}")).map(|value| (format!("TRACEVAULT_{name}"), value))
        })
        .collect()
}

/// Copy every set `VISDOM_X` over `TRACEVAULT_X`, so the library — and any
/// `tracevault` hook the launched agent runs — sees it. `VISDOM_X` wins.
///
/// Must run before any other thread exists (first thing in `main`).
pub fn apply() {
    for (name, value) in alias_pairs(|name| std::env::var(name).ok()) {
        std::env::set_var(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visdom_var_maps_to_tracevault() {
        let pairs = alias_pairs(|name| match name {
            "VISDOM_SERVER_URL" => Some("https://tv.example.com".to_string()),
            "VISDOM_API_KEY" => Some("tvk_x".to_string()),
            _ => None,
        });
        assert_eq!(
            pairs,
            vec![
                (
                    "TRACEVAULT_SERVER_URL".to_string(),
                    "https://tv.example.com".to_string()
                ),
                ("TRACEVAULT_API_KEY".to_string(), "tvk_x".to_string()),
            ]
        );
    }

    #[test]
    fn unset_visdom_var_leaves_tracevault_alone() {
        // Only VISDOM_* names are ever looked up, and none is set.
        let pairs = alias_pairs(|name| {
            assert!(name.starts_with("VISDOM_"), "looked up {name}");
            None
        });
        assert!(pairs.is_empty());
    }
}

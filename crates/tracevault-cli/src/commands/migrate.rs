//! Directory-scoped project binding (VIS-1256): resolving a project NAME to
//! the `project_id` a `.tracevault/config.toml` carries, and migrating the
//! legacy `default_project` name to it.
//!
//! The workspace tier of the capture chain reads `project_id` (a UUID)
//! locally, so the per-event stream hook never needs the network. A NAME is
//! resolved exactly once, by a command that already talks to the server and
//! may write config: `tracevault init` (any mode that has a project config)
//! and `tracevault sync`. Ingest never resolves names; until a config is
//! migrated its `default_project` contributes nothing, and `project status` /
//! `status` say so ([`legacy_default_project_notice`]).

use crate::api_client::{ApiClient, ProjectListItem};
use crate::config::TracevaultConfig;

/// Exact, case-sensitive name → the one project carrying it. An unknown
/// name lists the available ones; a name several projects share is an
/// error naming their ids, never a guess.
pub fn resolve_unique_project<'a>(
    items: &'a [ProjectListItem],
    name: &str,
) -> Result<&'a ProjectListItem, String> {
    let matches: Vec<&ProjectListItem> = items.iter().filter(|p| p.name == name).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => {
            let mut names: Vec<&str> = items.iter().map(|p| p.name.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            Err(format!(
                "project '{name}' not found. Available: {}",
                if names.is_empty() {
                    "(none visible to this account)".to_string()
                } else {
                    names.join(", ")
                }
            ))
        }
        many => {
            let ids: Vec<String> = many.iter().map(|p| p.id.to_string()).collect();
            Err(format!(
                "project name '{name}' is ambiguous: {} projects carry it ({}); set \
                 `project_id = \"<id>\"` in .tracevault/config.toml by hand",
                many.len(),
                ids.join(", ")
            ))
        }
    }
}

/// Why a name could not be turned into a `project_id`.
#[derive(Debug)]
pub enum ResolveNameError {
    /// The project list could not be fetched (network, auth, 5xx). The
    /// caller decides whether that stops it; the config is left unchanged.
    Unreachable(String),
    /// The server answered, and the name is unknown or ambiguous. Always an
    /// error: the config is left unchanged.
    Name(String),
}

impl std::fmt::Display for ResolveNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveNameError::Unreachable(e) => write!(f, "could not list projects: {e}"),
            ResolveNameError::Name(e) => f.write_str(e),
        }
    }
}

/// Look `name` up once against the server's project list.
pub async fn resolve_project_by_name(
    client: &ApiClient,
    name: &str,
) -> Result<ProjectListItem, ResolveNameError> {
    let items = client
        .list_projects()
        .await
        .map_err(|e| ResolveNameError::Unreachable(e.to_string()))?;
    resolve_unique_project(&items, name)
        .map(|p| ProjectListItem {
            id: p.id,
            name: p.name.clone(),
        })
        .map_err(ResolveNameError::Name)
}

/// What [`migrate_default_project`] did to the in-memory config.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// No legacy `default_project` in the config: nothing to do.
    NotNeeded,
    /// `default_project` was resolved and replaced by `project_id` +
    /// `project_name`.
    Migrated {
        name: String,
        project_id: uuid::Uuid,
    },
    /// The config already had a `project_id`, which is authoritative; the
    /// stale `default_project` was dropped without a lookup.
    DroppedLegacy { name: String },
}

/// Migrate a config's legacy `default_project` NAME to `project_id` +
/// `project_name`, in memory: the caller saves. On any error the config is
/// left exactly as it was.
///
/// A config that already carries a `project_id` keeps it (it is what the
/// capture chain already reads) and only loses the legacy key — no lookup.
pub async fn migrate_default_project(
    config: &mut TracevaultConfig,
    client: &ApiClient,
) -> Result<Migration, ResolveNameError> {
    let Some(name) = config.default_project.clone() else {
        return Ok(Migration::NotNeeded);
    };
    if crate::resolution::project_from_config(config).is_some() {
        config.default_project = None;
        return Ok(Migration::DroppedLegacy { name });
    }
    let project = resolve_project_by_name(client, &name)
        .await
        .map_err(|e| match e {
            ResolveNameError::Name(msg) => {
                ResolveNameError::Name(format!("cannot migrate default_project '{name}': {msg}"))
            }
            other => other,
        })?;
    config.project_id = Some(project.id.to_string());
    config.project_name = Some(project.name);
    config.default_project = None;
    Ok(Migration::Migrated {
        name,
        project_id: project.id,
    })
}

/// The one line a command prints after a migration, or `None` when nothing
/// changed.
pub fn migration_report(migration: &Migration) -> Option<String> {
    match migration {
        Migration::NotNeeded => None,
        Migration::Migrated { name, project_id } => Some(format!(
            "Migrated .tracevault/config.toml: default_project '{name}' -> project_id {project_id}"
        )),
        Migration::DroppedLegacy { name } => Some(format!(
            "Removed the legacy default_project '{name}' from .tracevault/config.toml \
             (its project_id is what attribution uses)"
        )),
    }
}

/// The status notice for a config that still carries the legacy
/// `default_project`: the name is never used for capture, so `project
/// status` and `status` say so instead of reporting it as effective (VIS-316:
/// status must match ingest). `None` when the config has no legacy key.
pub fn legacy_default_project_notice(config: &TracevaultConfig) -> Option<String> {
    let name = config.default_project.as_deref()?;
    Some(
        if crate::resolution::project_from_config(config).is_some() {
            format!(
                "default_project \"{name}\" in .tracevault/config.toml is ignored (its project_id \
             wins); run `tracevault sync` to remove it"
            )
        } else {
            format!(
                "default_project \"{name}\" is not used for capture until migrated; run \
             `tracevault sync`"
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{http_json, spawn_seq};

    fn item(id: u128, name: &str) -> ProjectListItem {
        ProjectListItem {
            id: uuid::Uuid::from_u128(id),
            name: name.into(),
        }
    }

    #[test]
    fn resolve_unique_project_exact_unknown_and_ambiguous() {
        let items = vec![item(1, "visdom"), item(2, "web"), item(3, "web")];
        assert_eq!(
            resolve_unique_project(&items, "visdom").unwrap().id,
            uuid::Uuid::from_u128(1)
        );
        let unknown = resolve_unique_project(&items, "Visdom").unwrap_err();
        assert!(unknown.contains("'Visdom' not found"), "{unknown}");
        assert!(unknown.contains("visdom, web"), "{unknown}");
        let ambiguous = resolve_unique_project(&items, "web").unwrap_err();
        assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
        assert!(
            ambiguous.contains(&uuid::Uuid::from_u128(2).to_string())
                && ambiguous.contains(&uuid::Uuid::from_u128(3).to_string()),
            "{ambiguous}"
        );
    }

    const LIST: &str = r#"[{"id":"00000000-0000-4000-8000-0000000000aa","name":"visdom"},{"id":"00000000-0000-4000-8000-0000000000bb","name":"dup"},{"id":"00000000-0000-4000-8000-0000000000cc","name":"dup"}]"#;

    #[tokio::test]
    async fn migrate_resolves_the_name_and_drops_the_legacy_key() {
        let (base, _rx) = spawn_seq(vec![http_json("200 OK", LIST)]);
        let client = ApiClient::with_credential(&base, None);
        let mut cfg = TracevaultConfig {
            default_project: Some("visdom".into()),
            ..Default::default()
        };
        let got = migrate_default_project(&mut cfg, &client).await.unwrap();
        let id: uuid::Uuid = "00000000-0000-4000-8000-0000000000aa".parse().unwrap();
        assert_eq!(
            got,
            Migration::Migrated {
                name: "visdom".into(),
                project_id: id
            }
        );
        assert_eq!(cfg.project_id.as_deref(), Some(id.to_string().as_str()));
        assert_eq!(cfg.project_name.as_deref(), Some("visdom"));
        assert_eq!(cfg.default_project, None);
    }

    #[tokio::test]
    async fn migrate_unknown_or_ambiguous_name_errors_and_leaves_config_unchanged() {
        for name in ["nope", "dup"] {
            let (base, _rx) = spawn_seq(vec![http_json("200 OK", LIST)]);
            let client = ApiClient::with_credential(&base, None);
            let mut cfg = TracevaultConfig {
                default_project: Some(name.into()),
                ..Default::default()
            };
            let err = migrate_default_project(&mut cfg, &client)
                .await
                .unwrap_err();
            assert!(matches!(err, ResolveNameError::Name(_)), "{err}");
            assert!(err.to_string().contains(&format!("'{name}'")), "{err}");
            assert_eq!(cfg.default_project.as_deref(), Some(name));
            assert_eq!(cfg.project_id, None);
            assert_eq!(cfg.project_name, None);
        }
    }

    #[tokio::test]
    async fn migrate_unreachable_server_leaves_config_unchanged() {
        let (base, _rx) = spawn_seq(vec![http_json("500 Internal Server Error", "{}")]);
        let client = ApiClient::with_credential(&base, None);
        let mut cfg = TracevaultConfig {
            default_project: Some("visdom".into()),
            ..Default::default()
        };
        let err = migrate_default_project(&mut cfg, &client)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolveNameError::Unreachable(_)), "{err}");
        assert_eq!(cfg.default_project.as_deref(), Some("visdom"));
        assert_eq!(cfg.project_id, None);
    }

    #[tokio::test]
    async fn migrate_with_project_id_present_drops_legacy_without_a_lookup() {
        // No server at all: a request would fail the test with Unreachable.
        let client = ApiClient::with_credential("http://127.0.0.1:9", None);
        let mut cfg = TracevaultConfig {
            default_project: Some("old".into()),
            project_id: Some("00000000-0000-4000-8000-0000000000aa".into()),
            project_name: Some("visdom".into()),
            ..Default::default()
        };
        let got = migrate_default_project(&mut cfg, &client).await.unwrap();
        assert_eq!(got, Migration::DroppedLegacy { name: "old".into() });
        assert_eq!(cfg.default_project, None);
        assert_eq!(cfg.project_name.as_deref(), Some("visdom"));
    }

    #[tokio::test]
    async fn migrate_without_legacy_key_is_a_no_op() {
        let client = ApiClient::with_credential("http://127.0.0.1:9", None);
        let mut cfg = TracevaultConfig::default();
        assert_eq!(
            migrate_default_project(&mut cfg, &client).await.unwrap(),
            Migration::NotNeeded
        );
    }

    #[test]
    fn legacy_notice_unmigrated_vs_shadowed() {
        let mut cfg = TracevaultConfig {
            default_project: Some("visdom".into()),
            ..Default::default()
        };
        assert_eq!(
            legacy_default_project_notice(&cfg).unwrap(),
            "default_project \"visdom\" is not used for capture until migrated; run `tracevault sync`"
        );
        cfg.project_id = Some("00000000-0000-4000-8000-0000000000aa".into());
        assert!(legacy_default_project_notice(&cfg)
            .unwrap()
            .contains("ignored (its project_id wins)"));
        cfg.default_project = None;
        assert_eq!(legacy_default_project_notice(&cfg), None);
    }
}

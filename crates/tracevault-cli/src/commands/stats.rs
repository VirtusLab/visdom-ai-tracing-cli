use std::fs;
use std::path::Path;

pub fn show_stats(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let sessions_dirs = crate::layout::existing_sessions_dirs(project_root);
    if sessions_dirs.is_empty() {
        println!("No sessions found. Run `tracevault init` first.");
        return Ok(());
    }

    let mut total_sessions = 0;
    let mut total_events = 0;

    let entries = sessions_dirs
        .iter()
        .map(fs::read_dir)
        .collect::<Result<Vec<_>, _>>()?;
    for entry in entries.into_iter().flatten() {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            total_sessions += 1;
            let events_path = entry.path().join("events.jsonl");
            if events_path.exists() {
                let content = fs::read_to_string(&events_path)?;
                total_events += content.lines().count();
            }
        }
    }

    println!("TraceVault Stats");
    println!("================");
    println!("Sessions:     {total_sessions}");
    println!("Total events: {total_events}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{add_worktree, init_git_repo};

    #[test]
    fn show_stats_ok_when_resolved_from_sibling_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("sibling-wt");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        // Seed a session under the PRIMARY .visdom/.
        let sess = repo.join(".visdom").join("sessions").join("s1");
        std::fs::create_dir_all(&sess).unwrap();

        // Resolving from the sibling must land on the primary root, where stats finds data.
        let root = crate::paths::resolve_project_root(&wt).root;
        assert!(
            show_stats(&root).is_ok(),
            "stats must succeed when resolved to the primary root from a sibling worktree"
        );
    }
}

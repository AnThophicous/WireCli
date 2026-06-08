use crate::config::PermissionMode;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) fn display_rel(workspace: &Path, path: &Path) -> String {
    if path == workspace {
        ".".to_string()
    } else if let Ok(rel) = path.strip_prefix(workspace) {
        rel.display().to_string()
    } else {
        path.display().to_string()
    }
}

pub(crate) fn resolve_within_workspace(
    workspace: &Path,
    relative_path: &str,
) -> Result<PathBuf, String> {
    resolve_with_permission(workspace, workspace, relative_path, PermissionMode::Normal)
}

pub(crate) fn resolve_with_permission(
    workspace: &Path,
    current_dir: &Path,
    relative_path: &str,
    permission_mode: PermissionMode,
) -> Result<PathBuf, String> {
    let candidate = Path::new(relative_path);
    if permission_mode != PermissionMode::FullAccess {
        if let Some(resolved) = workspace_alias_path(workspace, candidate) {
            ensure_canonical_inside_workspace(workspace, &resolved)?;
            return Ok(resolved);
        }
    }
    let mut resolved = if candidate.is_absolute() {
        PathBuf::from("/")
    } else {
        current_dir.to_path_buf()
    };

    for component in candidate.components() {
        match component {
            Component::RootDir => resolved = PathBuf::from("/"),
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if permission_mode != PermissionMode::FullAccess && resolved == workspace {
                    return Err("path escapes the box workspace".to_string());
                }
                if !resolved.pop() {
                    resolved = PathBuf::from("/");
                }
                if permission_mode != PermissionMode::FullAccess && !resolved.starts_with(workspace)
                {
                    return Err("path escapes the box workspace".to_string());
                }
            }
            _ => return Err("unsupported path component".to_string()),
        }
    }

    if permission_mode != PermissionMode::FullAccess {
        if !resolved.starts_with(workspace) {
            if candidate.is_absolute() {
                return Err("absolute paths are blocked outside the box workspace".to_string());
            }
            return Err("path escapes the box workspace".to_string());
        }
        ensure_canonical_inside_workspace(workspace, &resolved)?;
    }

    Ok(resolved)
}

pub(crate) fn resolve_existing_project_path(
    workspace: &Path,
    current_dir: &Path,
    relative_path: &str,
    permission_mode: PermissionMode,
) -> Result<PathBuf, String> {
    let resolved = resolve_with_permission(workspace, current_dir, relative_path, permission_mode)?;
    if resolved.exists() || Path::new(relative_path).is_absolute() {
        return Ok(resolved);
    }

    let project_relative =
        resolve_with_permission(workspace, workspace, relative_path, permission_mode)?;
    if project_relative.exists() {
        return Ok(project_relative);
    }
    Ok(resolved)
}

pub(crate) fn resolve_within_workspace_from(
    workspace: &Path,
    current_dir: &Path,
    relative_path: &str,
) -> Result<PathBuf, String> {
    let current = if current_dir.as_os_str().is_empty() {
        workspace.to_path_buf()
    } else if current_dir.is_absolute() {
        current_dir.to_path_buf()
    } else {
        workspace.join(current_dir)
    };
    resolve_with_permission(workspace, &current, relative_path, PermissionMode::Normal)
}

pub(crate) fn resolve_patch_path(
    workspace: &Path,
    current_dir: &Path,
    permission_mode: PermissionMode,
    relative_path: &str,
) -> Result<PathBuf, String> {
    let candidate = Path::new(relative_path);
    if permission_mode != PermissionMode::FullAccess
        && candidate.is_absolute()
        && !candidate.starts_with(workspace)
    {
        return Err("absolute paths are blocked outside the box workspace".to_string());
    }
    resolve_with_permission(workspace, current_dir, relative_path, permission_mode)
}

fn ensure_canonical_inside_workspace(workspace: &Path, resolved: &Path) -> Result<(), String> {
    let workspace = fs::canonicalize(workspace).map_err(|e| {
        format!(
            "failed to canonicalize box workspace {}: {e}",
            workspace.display()
        )
    })?;
    let target = canonical_existing_target(resolved)?;
    if !target.starts_with(&workspace) {
        return Err("path escapes the box workspace through a symlink".to_string());
    }
    Ok(())
}

fn workspace_alias_path(workspace: &Path, path: &Path) -> Option<PathBuf> {
    path.strip_prefix("/workspace")
        .ok()
        .map(|relative| workspace.join(relative))
}

fn canonical_existing_target(path: &Path) -> Result<PathBuf, String> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Ok(canonical);
    }

    let mut current = path;
    while let Some(parent) = current.parent() {
        if let Ok(canonical) = fs::canonicalize(parent) {
            return Ok(canonical);
        }
        current = parent;
    }

    Err(format!(
        "failed to canonicalize path or parent: {}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::{resolve_existing_project_path, resolve_with_permission, resolve_within_workspace};
    use crate::config::PermissionMode;
    use std::fs;

    #[test]
    fn resolves_relative_paths() {
        let root = std::env::temp_dir().join("wire_tools_path_test");
        fs::create_dir_all(&root).unwrap();
        let resolved = resolve_within_workspace(&root, "src/lib.rs").unwrap();
        assert!(resolved.starts_with(&root));
    }

    #[test]
    fn resolves_workspace_absolute_alias() {
        let root = std::env::temp_dir().join("wire_tools_workspace_alias_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();

        let resolved = resolve_with_permission(
            &root,
            &root,
            "/workspace/src/lib.rs",
            PermissionMode::Normal,
        )
        .unwrap();

        assert_eq!(resolved, root.join("src/lib.rs"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_host_absolute_path_inside_workspace() {
        let root = std::env::temp_dir().join("wire_tools_host_absolute_path_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/lib.rs");
        fs::write(&file, "").unwrap();

        let resolved = resolve_with_permission(
            &root,
            &root,
            &file.display().to_string(),
            PermissionMode::Normal,
        )
        .unwrap();

        assert_eq!(resolved, file);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_project_path_wins_from_subdirectory() {
        let root = std::env::temp_dir().join("wire_tools_project_relative_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();

        let resolved = resolve_existing_project_path(
            &root,
            &root.join("src"),
            "src/lib.rs",
            PermissionMode::Normal,
        )
        .unwrap();

        assert_eq!(resolved, root.join("src/lib.rs"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn blocks_missing_file_under_symlink_to_outside_workspace() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("wire_tools_symlink_workspace");
        let outside = std::env::temp_dir().join("wire_tools_symlink_outside");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("outside")).unwrap();

        let err = resolve_within_workspace(&root, "outside/new.txt").unwrap_err();
        assert!(err.contains("symlink"));

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }
}

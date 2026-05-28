// Resolve the Julia project path for a client request.

use crate::args::ParsedArgs;

pub fn resolve(parsed: &ParsedArgs, julia_project: Option<&str>, home_dir: &str, cwd: &str) -> Option<String> {
    // 1. Check --project switch (last occurrence wins)
    if let Some(project) = parsed.get_switch("--project") {
        if project.is_empty() || project == "@." {
            return find_project_toml(cwd);
        }
        return Some(project.to_string());
    }

    // 2. Check JULIA_PROJECT env var
    if let Some(project) = julia_project {
        if project.is_empty() || project == "@." {
            return find_project_toml(cwd);
        }
        if project.starts_with("~/") && !home_dir.is_empty() {
            return Some(format!("{}{}", home_dir, &project[1..]));
        }
        if !std::path::Path::new(project).is_absolute() {
            let joined = std::path::Path::new(cwd).join(project);
            return joined.to_str().map(|s| s.to_string());
        }
        return Some(project.to_string());
    }

    None
}

fn find_project_toml(start: &str) -> Option<String> {
    let mut dir = std::path::Path::new(start);
    loop {
        if dir.join("Project.toml").exists() {
            return dir.to_str().map(|s| s.to_string());
        }
        let parent = dir.parent()?;
        if parent == dir { return None; }
        dir = parent;
    }
}

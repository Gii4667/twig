use anyhow::{Context, Result};
use std::fs;

use crate::config::{GlobalConfig, Project};
use crate::gum;

pub fn run(name: Option<String>) -> Result<()> {
    GlobalConfig::ensure_dirs()?;

    // Get project name or repo URL
    let input = match name {
        Some(n) => n,
        None => match gum::input("Project name or repo URL", None)? {
            Some(n) if !n.is_empty() => n,
            _ => anyhow::bail!("Project name or repo URL is required"),
        },
    };

    // Check if input is a git URL
    let (project_name, repo_url) = if Project::is_git_url(&input) {
        let name = Project::name_from_repo_url(&input)
            .ok_or_else(|| anyhow::anyhow!("Could not extract project name from URL: {}", input))?;
        (name, Some(input))
    } else {
        (input, None)
    };

    // Check if project already exists
    let config_path = Project::config_path(&project_name)?;
    if config_path.exists() {
        anyhow::bail!(
            "Project '{}' already exists at {:?}",
            project_name,
            config_path
        );
    }

    // Get project root
    let default_root = format!("~/Work/{}", project_name);
    let root = match gum::input("Project root", Some(&default_root))? {
        Some(r) if !r.is_empty() => r,
        _ => default_root,
    };

    // Generate config content
    let config_content = if let Some(ref url) = repo_url {
        format!(
            r#"name: {}
root: {}
repo: {}

windows:
  - editor:
      panes:
        - nvim
  - shell:
  - shell:
  - git: lazygit

# Worktree configuration (optional)
# worktree:
#   copy:
#     - .env
#     - .env.local
#   post_create:
#     - bundle install
#     - yarn install
"#,
            project_name, root, url
        )
    } else {
        format!(
            r#"name: {}
root: {}

windows:
  - editor:
      panes:
        - nvim
  - shell:
  - shell:
  - git: lazygit

# Worktree configuration (optional)
# worktree:
#   copy:
#     - .env
#     - .env.local
#   post_create:
#     - bundle install
#     - yarn install
"#,
            project_name, root
        )
    };

    // Write the config file
    fs::write(&config_path, &config_content)
        .with_context(|| format!("Failed to write config: {:?}", config_path))?;

    println!("Created project config: {:?}", config_path);
    if repo_url.is_some() {
        println!("Repository will be cloned on first start.");
    }
    println!();
    println!("Edit it with: twig edit {}", project_name);
    println!("Start it with: twig start {}", project_name);

    Ok(())
}

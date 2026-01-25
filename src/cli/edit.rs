use anyhow::{Context, Result};
use std::process::Command;

use crate::config::Project;
use crate::ui;

pub fn run(project_name: Option<String>) -> Result<()> {
    let name = match project_name {
        Some(n) => n,
        None => ui::select_project("Select project to edit...")?
            .ok_or_else(|| anyhow::anyhow!("No project selected"))?,
    };

    let config_path = Project::config_path(&name)?;

    if !config_path.exists() {
        anyhow::bail!(
            "Project '{}' not found. Create it with: twig new {}",
            name,
            name
        );
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    Command::new(&editor)
        .arg(&config_path)
        .status()
        .with_context(|| format!("Failed to open editor: {}", editor))?;

    Ok(())
}

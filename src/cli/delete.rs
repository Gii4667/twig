use anyhow::Result;

use crate::config::Project;
use crate::ui;

pub fn run(project_name: Option<String>) -> Result<()> {
    let name = match project_name {
        Some(n) => n,
        None => ui::select_project("Select project to delete...")?
            .ok_or_else(|| anyhow::anyhow!("No project selected"))?,
    };

    let config_path = Project::config_path(&name)?;

    if !config_path.exists() {
        anyhow::bail!("Project '{}' not found", name);
    }

    // Confirm deletion
    if !ui::confirm(&format!("Delete project '{}'?", name))? {
        println!("Cancelled.");
        return Ok(());
    }

    Project::delete(&name)?;
    println!("Deleted project: {}", name);

    Ok(())
}

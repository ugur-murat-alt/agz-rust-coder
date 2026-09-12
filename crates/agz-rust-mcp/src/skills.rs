//! Versioned, offline workflow guidance shipped in the executable.
use std::{fs, io, path::Path};

#[derive(Debug, Clone, Copy)]
pub struct BundledSkill {
    pub name: &'static str,
    pub prompt: &'static str,
    pub description: &'static str,
    pub uri: &'static str,
    pub markdown: &'static str,
}

pub const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "agz-rust-workflow",
        prompt: "workflow",
        description: "Implement Rust changes with focused context and compiler validation.",
        uri: "agz-rust-mcp://skills/agz-rust-workflow",
        markdown: include_str!("../skills/agz-rust-workflow/SKILL.md"),
    },
    BundledSkill {
        name: "agz-rust-repair",
        prompt: "repair",
        description: "Diagnose compiler failures and validate bounded repair candidates.",
        uri: "agz-rust-mcp://skills/agz-rust-repair",
        markdown: include_str!("../skills/agz-rust-repair/SKILL.md"),
    },
    BundledSkill {
        name: "agz-rust-refactor",
        prompt: "refactor",
        description: "Refactor and remove unnecessary code with consumer evidence.",
        uri: "agz-rust-mcp://skills/agz-rust-refactor",
        markdown: include_str!("../skills/agz-rust-refactor/SKILL.md"),
    },
    BundledSkill {
        name: "agz-rust-performance",
        prompt: "performance",
        description: "Measure comparable build or runtime performance and correctness.",
        uri: "agz-rust-mcp://skills/agz-rust-performance",
        markdown: include_str!("../skills/agz-rust-performance/SKILL.md"),
    },
];

pub fn run(command: &crate::config::SkillCommand) -> io::Result<()> {
    use crate::config::SkillCommand;
    use io::Write;
    let mut output = io::stdout().lock();
    match command {
        SkillCommand::List => {
            for skill in BUNDLED_SKILLS {
                writeln!(output, "{}\t{}", skill.name, skill.description)?;
            }
        }
        SkillCommand::Show { name } => {
            let skill = BUNDLED_SKILLS
                .iter()
                .find(|skill| skill.name == name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown skill"))?;
            output.write_all(skill.markdown.as_bytes())?;
        }
        SkillCommand::Export { dir } => {
            export(dir).map_err(|error| {
                io::Error::new(error.kind(), format!(
                    "skill export to {} failed (destination must be new; any partial output is left for inspection): {error}",
                    dir.display()
                ))
            })?;
            writeln!(
                output,
                "Exported {} skills to {}",
                BUNDLED_SKILLS.len(),
                dir.display()
            )?;
        }
    }
    Ok(())
}

/// Export only into a new destination. Existing custom skills are never replaced.
/// The caller chooses a trusted parent directory; no MCP request invokes this.
pub fn export(destination: &Path) -> io::Result<()> {
    fs::create_dir(destination)?;
    for skill in BUNDLED_SKILLS {
        let folder = destination.join(skill.name);
        fs::create_dir(&folder)?;
        let mut file = fs::File::create_new(folder.join("SKILL.md"))?;
        io::Write::write_all(&mut file, skill.markdown.as_bytes())?;
    }
    Ok(())
}

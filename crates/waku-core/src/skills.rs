//! Agent-skill discovery and management for the Skills settings page.
//!
//! A skill is a directory holding a `SKILL.md` — reusable instructions any
//! coding agent can load. Every ecosystem keeps its own skill roots (see
//! [`crate::composer_complete::discover_slash_commands`] for the invocation
//! side); this module walks all of them at once so the settings page can show
//! one library across providers and projects.
//!
//! Installers routinely drop the same skill into several ecosystems' roots,
//! and dotfile setups symlink one directory everywhere. The catalog therefore
//! groups by name within a scope: one [`SkillEntry`] per skill, carrying
//! every [`SkillInstall`] it was found at, so the library lists each skill
//! once and mutations apply to all of its copies.
//!
//! Discovery reads directories and files, so it runs on the background
//! executor only. Mutations — create, enable, disable — are one-shot user
//! actions and may run synchronously in a click handler; each is a rename,
//! write, or mkdir per install.
//!
//! Disabling renames `SKILL.md` to `SKILL.md.disabled`. Every tool discovers
//! skills by that exact filename, so the rename hides the skill from all of
//! them at once while keeping the directory and its supporting files intact —
//! the same move people make by hand, made reversible with one toggle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::ProviderKind;

pub use waku_protocol::skills::{
    DISABLED_SKILL_FILE, SKILL_FILE, SkillEntry, SkillInstall, SkillLocation, SkillScope,
    SkillSource, SkillsCatalog,
};

/// Upper bound on scanned skill directories; past this the library is
/// best-effort.
const SCAN_CAP: usize = 500;
const SKILL_FILE_MAX_BYTES: u64 = 256 * 1024;
/// Bounds for the per-skill supporting-file walk. Deep trees stop counting
/// rather than stall the scan; the sizes shown degrade to "at least this".
const DIR_WALK_MAX_DEPTH: usize = 6;
const DIR_WALK_MAX_FILES: usize = 500;

/// Every user-scope skill root, present on disk or not. Path joins only — no
/// filesystem access — so this is safe to call while building a frame.
pub fn user_skill_locations() -> Vec<SkillLocation> {
    let home = dirs::home_dir();
    let claude_config_dir_env = crate::command_env::env_var_os("CLAUDE_CONFIG_DIR");
    let codex_home_env = crate::command_env::env_var_os("CODEX_HOME");
    let antigravity_data_dir_env =
        crate::command_env::env_var_os("ANTIGRAVITY_EXECUTABLE_DATA_DIR");
    user_skill_locations_with(
        home.as_deref(),
        claude_config_dir_env.as_deref(),
        codex_home_env.as_deref(),
        antigravity_data_dir_env.as_deref(),
    )
}

fn antigravity_data_root(path: &Path) -> Option<PathBuf> {
    if path.file_name().and_then(|name| name.to_str()) == Some("sidecar_data") {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
    } else {
        Some(path.to_path_buf())
    }
}

fn push_unique_location(locations: &mut Vec<SkillLocation>, location: SkillLocation) {
    if !locations.contains(&location) {
        locations.push(location);
    }
}

fn push_antigravity_user_roots(
    locations: &mut Vec<SkillLocation>,
    data_root: &Path,
) {
    for suffix in ["config/skills", "skills"] {
        push_unique_location(
            locations,
            SkillLocation {
                source: SkillSource::Provider(ProviderKind::Antigravity),
                scope: SkillScope::User,
                root: data_root.join(suffix),
                project: None,
            },
        );
    }
}

fn user_skill_locations_with(
    home: Option<&Path>,
    claude_config_dir_env: Option<&std::ffi::OsStr>,
    codex_home_env: Option<&std::ffi::OsStr>,
    antigravity_data_dir_env: Option<&std::ffi::OsStr>,
) -> Vec<SkillLocation> {
    let claude_config_dir = claude_config_dir_env
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".claude")));
    let codex_config_dir = codex_home_env
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".codex")));
    let antigravity_data_dir = antigravity_data_dir_env
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| antigravity_data_root(&path));
    let mut locations = Vec::new();
    let mut push = |source: SkillSource, root: Option<PathBuf>| {
        if let Some(root) = root {
            push_unique_location(
                &mut locations,
                SkillLocation {
                    source,
                    scope: SkillScope::User,
                    root,
                    project: None,
                },
            );
        }
    };
    let home_join = |suffix: &str| home.map(|home| home.join(suffix));
    push(SkillSource::Shared, home_join(".agents/skills"));
    push(
        SkillSource::Provider(ProviderKind::Claude),
        claude_config_dir.map(|dir| dir.join("skills")),
    );
    push(
        SkillSource::Provider(ProviderKind::Codex),
        codex_config_dir.map(|dir| dir.join("skills")),
    );
    push(
        SkillSource::Provider(ProviderKind::OpenCode),
        home_join(".config/opencode/skills"),
    );
    push(
        SkillSource::Provider(ProviderKind::Cursor),
        home_join(".cursor/skills"),
    );
    push(
        SkillSource::Provider(ProviderKind::Fx),
        home_join(".fx/skills"),
    );
    push(
        SkillSource::Provider(ProviderKind::Pi),
        home_join(".pi/agent/skills"),
    );
    push(
        SkillSource::Provider(ProviderKind::OhMyPi),
        home_join(".omp/agent/skills"),
    );
    push(
        SkillSource::Provider(ProviderKind::Amp),
        home_join(".config/agents/skills"),
    );
    push(
        SkillSource::Provider(ProviderKind::Droid),
        home_join(".factory/skills"),
    );
    drop(push);
    if let Some(data_dir) = antigravity_data_dir {
        push_antigravity_user_roots(&mut locations, &data_dir);
    }
    // `agy` merges the shared Gemini profile with its CLI-private skills.
    for suffix in [".gemini/config/skills", ".gemini/antigravity-cli/skills"] {
        if let Some(root) = home_join(suffix) {
            push_unique_location(
                &mut locations,
                SkillLocation {
                    source: SkillSource::Provider(ProviderKind::Antigravity),
                    scope: SkillScope::User,
                    root,
                    project: None,
                },
            );
        }
    }
    locations
}

/// Every project-scope skill root under `project_root`. Path joins only.
pub fn project_skill_locations(project_root: &Path, project_name: &str) -> Vec<SkillLocation> {
    [
        (SkillSource::Shared, ".agents/skills"),
        (
            SkillSource::Provider(ProviderKind::Claude),
            ".claude/skills",
        ),
        (SkillSource::Provider(ProviderKind::Codex), ".codex/skills"),
        (
            SkillSource::Provider(ProviderKind::OpenCode),
            ".opencode/skills",
        ),
        (
            SkillSource::Provider(ProviderKind::Cursor),
            ".cursor/skills",
        ),
        (SkillSource::Provider(ProviderKind::Fx), "skills"),
        (SkillSource::Provider(ProviderKind::Pi), ".pi/skills"),
        (SkillSource::Provider(ProviderKind::OhMyPi), ".omp/skills"),
        (
            SkillSource::Provider(ProviderKind::Droid),
            ".factory/skills",
        ),
        (
            SkillSource::Provider(ProviderKind::Antigravity),
            ".gemini/skills",
        ),
        (
            SkillSource::Provider(ProviderKind::Antigravity),
            ".agent/skills",
        ),
        (
            SkillSource::Provider(ProviderKind::Antigravity),
            "_agents/skills",
        ),
        (
            SkillSource::Provider(ProviderKind::Antigravity),
            "_agent/skills",
        ),
    ]
    .into_iter()
    .map(|(source, suffix)| SkillLocation {
        source,
        scope: SkillScope::Project,
        root: project_root.join(suffix),
        project: Some(project_name.to_owned()),
    })
    .collect()
}

/// All roots the scan walks for the given projects: user scope plus each
/// project's trees, in scan order.
pub fn skill_locations(projects: &[(String, PathBuf)]) -> Vec<SkillLocation> {
    skill_locations_for(projects, &[])
}

/// All roots the scan walks for the given projects and registered isolated
/// provider account directories: default user scope, isolated account user
/// roots, Antigravity `skills.json` declared roots, then each project's trees.
pub fn skill_locations_for(
    projects: &[(String, PathBuf)],
    account_dirs: &[(ProviderKind, PathBuf)],
) -> Vec<SkillLocation> {
    let home = dirs::home_dir();
    let antigravity_data_dir = crate::command_env::env_var_os("ANTIGRAVITY_EXECUTABLE_DATA_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| antigravity_data_root(&path));
    let mut locations = user_skill_locations();
    extend_account_skill_locations(&mut locations, account_dirs);
    extend_antigravity_skills_json_locations(
        &mut locations,
        home.as_deref(),
        antigravity_data_dir.as_deref(),
        projects,
        account_dirs,
    );
    for (name, path) in projects {
        for location in project_skill_locations(path, name) {
            push_unique_location(&mut locations, location);
        }
    }
    locations
}

fn extend_account_skill_locations(
    locations: &mut Vec<SkillLocation>,
    account_dirs: &[(ProviderKind, PathBuf)],
) {
    for (provider, config_dir) in account_dirs {
        match provider {
            ProviderKind::Antigravity => {
                if let Some(root) = antigravity_data_root(config_dir) {
                    push_antigravity_user_roots(locations, &root);
                }
            }
            ProviderKind::Claude | ProviderKind::Codex => {
                push_unique_location(
                    locations,
                    SkillLocation {
                        source: SkillSource::Provider(*provider),
                        scope: SkillScope::User,
                        root: config_dir.join("skills"),
                        project: None,
                    },
                );
            }
            _ => {}
        }
    }
}

const SKILLS_MANIFEST_FILE: &str = "skills.json";
const SKILLS_MANIFEST_MAX_BYTES: u64 = 64 * 1024;
const SKILLS_MANIFEST_MAX_DEPTH: usize = 8;

fn extend_antigravity_skills_json_locations(
    locations: &mut Vec<SkillLocation>,
    home: Option<&Path>,
    antigravity_data_dir: Option<&Path>,
    projects: &[(String, PathBuf)],
    account_dirs: &[(ProviderKind, PathBuf)],
) {
    let mut visited = std::collections::HashSet::new();
    let mut user_manifests = Vec::new();
    if let Some(home) = home {
        user_manifests.push(home.join(".gemini/config").join(SKILLS_MANIFEST_FILE));
    }
    if let Some(data_dir) = antigravity_data_dir {
        user_manifests.push(data_dir.join("config").join(SKILLS_MANIFEST_FILE));
    }
    for (provider, config_dir) in account_dirs {
        if *provider == ProviderKind::Antigravity
            && let Some(root) = antigravity_data_root(config_dir)
        {
            user_manifests.push(root.join("config").join(SKILLS_MANIFEST_FILE));
        }
    }
    for manifest in user_manifests {
        collect_skills_json_manifest(
            &manifest,
            None,
            home,
            SkillScope::User,
            None,
            0,
            &mut visited,
            locations,
        );
    }

    for (name, project_root) in projects {
        for dir_name in [".agents", ".agent", "_agents", "_agent", ".gemini"] {
            let manifest = project_root.join(dir_name).join(SKILLS_MANIFEST_FILE);
            collect_skills_json_manifest(
                &manifest,
                Some(project_root),
                home,
                SkillScope::Project,
                Some(name.as_str()),
                0,
                &mut visited,
                locations,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_skills_json_manifest(
    manifest_path: &Path,
    project_root: Option<&Path>,
    home: Option<&Path>,
    scope: SkillScope,
    project: Option<&str>,
    depth: usize,
    visited: &mut std::collections::HashSet<PathBuf>,
    locations: &mut Vec<SkillLocation>,
) {
    if depth >= SKILLS_MANIFEST_MAX_DEPTH {
        return;
    }
    let Ok(metadata) = std::fs::metadata(manifest_path) else {
        return;
    };
    if !metadata.is_file() || metadata.len() > SKILLS_MANIFEST_MAX_BYTES {
        return;
    }
    let canonical = std::fs::canonicalize(manifest_path).unwrap_or_else(|_| manifest_path.to_path_buf());
    if !visited.insert(canonical) {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(manifest_path) else {
        return;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return;
    };
    let base_dir = manifest_path.parent();
    for inherit in doc
        .get("inherits")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(raw_path) = inherit.get("path").and_then(serde_json::Value::as_str)
            && let Some(resolved) = resolve_skills_json_path(raw_path, base_dir, project_root, home)
        {
            collect_skills_json_manifest(
                &resolved,
                project_root,
                home,
                scope,
                project,
                depth + 1,
                visited,
                locations,
            );
        }
    }
    for entry in doc
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(raw_path) = entry.get("path").and_then(serde_json::Value::as_str)
            && let Some(resolved) = resolve_skills_json_path(raw_path, base_dir, project_root, home)
        {
            push_unique_location(
                locations,
                SkillLocation {
                    source: SkillSource::Provider(ProviderKind::Antigravity),
                    scope,
                    root: resolved,
                    project: project.map(str::to_owned),
                },
            );
        }
    }
}

fn resolve_skills_json_path(
    raw: &str,
    base_dir: Option<&Path>,
    project_root: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
        return home.map(|home_dir| home_dir.join(rest));
    }
    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }
    if let Some(root) = project_root {
        let from_root = root.join(candidate);
        if from_root.exists() {
            return Some(from_root);
        }
    }
    if let Some(base) = base_dir {
        let from_base = base.join(candidate);
        if from_base.exists() || project_root.is_none() {
            return Some(from_base);
        }
    }
    project_root.map(|root| root.join(candidate))
}

/// One skill directory as found on disk, before grouping.
struct RawSkill {
    name: String,
    description: String,
    scope: SkillScope,
    project: Option<String>,
    install: SkillInstall,
    allowed_tools: Option<String>,
    body: String,
    supporting_files: usize,
    total_bytes: u64,
    modified_at: Option<u64>,
}

/// Walk every location and build the catalog. Filesystem work throughout —
/// background executor only.
pub fn scan_skills(locations: &[SkillLocation]) -> SkillsCatalog {
    let mut raw = Vec::new();
    for location in locations {
        scan_location(location, &mut raw);
        if raw.len() >= SCAN_CAP {
            break;
        }
    }

    // One entry per (scope group, name): the same skill installed into
    // several ecosystems' roots — copied or symlinked — is one skill.
    let mut skills: Vec<SkillEntry> = Vec::new();
    let mut by_identity: HashMap<(Option<String>, String), usize> = HashMap::new();
    for raw in raw {
        let key = (raw.project.clone(), raw.name.clone());
        match by_identity.get(&key) {
            Some(&index) => {
                let entry = &mut skills[index];
                entry.enabled |= raw.install.enabled;
                if entry.description.is_empty() {
                    entry.description = raw.description;
                }
                if entry.allowed_tools.is_none() {
                    entry.allowed_tools = raw.allowed_tools;
                }
                if entry.body.is_empty() {
                    entry.body = raw.body;
                }
                entry.modified_at = entry.modified_at.max(raw.modified_at);
                entry.installs.push(raw.install);
            }
            None => {
                by_identity.insert(key, skills.len());
                skills.push(SkillEntry {
                    name: raw.name,
                    description: raw.description,
                    scope: raw.scope,
                    project: raw.project,
                    enabled: raw.install.enabled,
                    installs: vec![raw.install],
                    allowed_tools: raw.allowed_tools,
                    body: raw.body,
                    supporting_files: raw.supporting_files,
                    total_bytes: raw.total_bytes,
                    modified_at: raw.modified_at,
                    duplicates: 0,
                    row_key: 0,
                });
            }
        }
    }

    skills.sort_by(|a, b| {
        let group = |skill: &SkillEntry| (skill.project.clone(), skill.name.to_lowercase());
        group(a).cmp(&group(b))
    });

    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for skill in &skills {
        *name_counts.entry(skill.name.as_str()).or_default() += 1;
    }
    let duplicates = skills
        .iter()
        .map(|skill| name_counts[skill.name.as_str()] - 1)
        .collect::<Vec<_>>();
    for (skill, duplicates) in skills.iter_mut().zip(duplicates) {
        skill.duplicates = duplicates;
        skill.row_key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for install in &skill.installs {
                install.dir.hash(&mut hasher);
                install.enabled.hash(&mut hasher);
            }
            hasher.finish()
        };
    }
    SkillsCatalog { skills }
}

fn scan_location(location: &SkillLocation, raw: &mut Vec<RawSkill>) {
    let Ok(entries) = std::fs::read_dir(&location.root) else {
        return;
    };
    for entry in entries.flatten() {
        if raw.len() >= SCAN_CAP {
            return;
        }
        let dir = entry.path();
        // Follows symlinks — skills are routinely linked into place.
        if !dir.is_dir() {
            continue;
        }
        let Some(dir_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if dir_name.starts_with('.') {
            continue;
        }
        let (skill_file, enabled) = if dir.join(SKILL_FILE).is_file() {
            (dir.join(SKILL_FILE), true)
        } else if dir.join(DISABLED_SKILL_FILE).is_file() {
            (dir.join(DISABLED_SKILL_FILE), false)
        } else {
            continue;
        };
        if std::fs::metadata(&skill_file).is_ok_and(|meta| meta.len() > SKILL_FILE_MAX_BYTES) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&skill_file) else {
            continue;
        };
        let front = parse_skill_frontmatter(&contents);
        let modified_at = std::fs::metadata(&skill_file)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        let (supporting_files, total_bytes) = measure_skill_dir(&dir);
        let mut body_end = front.body.len().min(BODY_PREVIEW_MAX_BYTES);
        while !front.body.is_char_boundary(body_end) {
            body_end -= 1;
        }
        raw.push(RawSkill {
            name: front.name.unwrap_or(dir_name),
            description: front.description.unwrap_or_default(),
            scope: location.scope,
            project: location.project.clone(),
            install: SkillInstall {
                source: location.source,
                dir,
                skill_file,
                enabled,
            },
            allowed_tools: front.allowed_tools,
            body: front.body[..body_end].trim().to_owned(),
            supporting_files,
            total_bytes,
            modified_at,
        });
    }
}

/// `(files beside the skill file, total bytes)` for a skill directory,
/// bounded so a runaway tree cannot stall the scan.
fn measure_skill_dir(dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        if depth > DIR_WALK_MAX_DEPTH || files >= DIR_WALK_MAX_FILES {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            if files >= DIR_WALK_MAX_FILES {
                break;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            // `fs::metadata` follows symlinks; the depth cap bounds cycles.
            match std::fs::metadata(entry.path()) {
                Ok(meta) if meta.is_dir() => stack.push((entry.path(), depth + 1)),
                Ok(meta) if meta.is_file() => {
                    files += 1;
                    bytes += meta.len();
                }
                _ => {}
            }
        }
    }
    // The skill file itself is counted in bytes but not listed as a
    // supporting file.
    (files.saturating_sub(1), bytes)
}

/// Bytes of a skill's document kept on the entry for the detail pane. The
/// full file stays one click away; the catalog only carries a render-ready
/// preview so a pathological library cannot pin megabytes.
const BODY_PREVIEW_MAX_BYTES: usize = 32 * 1024;

struct SkillFrontmatter<'a> {
    name: Option<String>,
    description: Option<String>,
    allowed_tools: Option<String>,
    #[allow(dead_code)]
    disable_slash_command: bool,
    /// The document below the frontmatter block.
    body: &'a str,
}

/// Pull the keys the page shows out of a leading YAML block. The shared
/// frontmatter reader understands simple scalar values and block strings, and
/// skips unsupported lines so an extra key never costs the skill its listing.
fn parse_skill_frontmatter(contents: &str) -> SkillFrontmatter<'_> {
    let mut front = SkillFrontmatter {
        name: None,
        description: None,
        allowed_tools: None,
        disable_slash_command: false,
        body: contents,
    };
    front.body = crate::frontmatter::parse_frontmatter_fields(contents, |key, value| match key {
        "name" => front.name = Some(value),
        "description" => front.description = Some(value),
        "allowed-tools" => front.allowed_tools = Some(value),
        "disable-slash-command" => {
            front.disable_slash_command = value.eq_ignore_ascii_case("true");
        }
        _ => {}
    });
    front
}

/// Flip one copy's discoverability by renaming its skill file. Enabling a
/// copy that already has a live `SKILL.md` is a no-op rather than an error —
/// which also makes toggling a symlink-shared directory reached through
/// several installs idempotent.
pub fn set_skill_enabled(dir: &Path, enabled: bool) -> Result<(), String> {
    let live = dir.join(SKILL_FILE);
    let disabled = dir.join(DISABLED_SKILL_FILE);
    if enabled {
        if live.is_file() {
            return Ok(());
        }
        if !disabled.is_file() {
            return Ok(());
        }
        std::fs::rename(&disabled, &live).map_err(|error| error.to_string())
    } else {
        if !live.is_file() {
            return Ok(());
        }
        // `rename` replaces any stale `.disabled` leftover; the live file wins.
        std::fs::rename(&live, &disabled).map_err(|error| error.to_string())
    }
}

/// Move each concrete skill install to the daemon host's Trash.
pub fn trash_skills(dirs: &[PathBuf]) -> Result<(), String> {
    for dir in dirs {
        trash::delete(dir).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("waku-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_skill(root: &Path, dir: &str, contents: &str) {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join(SKILL_FILE), contents).unwrap();
    }

    fn user_location(source: SkillSource, root: &Path) -> SkillLocation {
        SkillLocation {
            source,
            scope: SkillScope::User,
            root: root.to_path_buf(),
            project: None,
        }
    }

    #[test]
    fn scan_reads_frontmatter_and_marks_disabled_entries() {
        let root = temp_root("scan");
        write_skill(
            &root,
            "deploy",
            "---\nname: deploy\ndescription: Ship it\nallowed-tools: Bash, Read\n---\nSteps…",
        );
        std::fs::create_dir_all(root.join("dormant")).unwrap();
        std::fs::write(
            root.join("dormant").join(DISABLED_SKILL_FILE),
            "---\nname: dormant\n---\nX",
        )
        .unwrap();
        // A directory without a skill file is not a skill.
        std::fs::create_dir_all(root.join("not-a-skill")).unwrap();
        // Supporting files count toward the entry's footprint.
        std::fs::write(root.join("deploy").join("runbook.md"), "details").unwrap();

        let locations = vec![user_location(SkillSource::Shared, &root)];
        let catalog = scan_skills(&locations);
        assert_eq!(catalog.skills.len(), 2);

        let deploy = catalog.skills.iter().find(|s| s.name == "deploy").unwrap();
        assert!(deploy.enabled);
        assert_eq!(deploy.description, "Ship it");
        assert_eq!(deploy.body, "Steps…");
        assert_eq!(deploy.allowed_tools.as_deref(), Some("Bash, Read"));
        assert_eq!(deploy.supporting_files, 1);
        assert!(deploy.total_bytes > 0);
        assert!(deploy.modified_at.is_some());
        assert_eq!(deploy.installs.len(), 1);
        assert_eq!(deploy.primary().source, SkillSource::Shared);

        let dormant = catalog.skills.iter().find(|s| s.name == "dormant").unwrap();
        assert!(!dormant.enabled);
        assert_eq!(catalog.disabled_count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_folds_block_scalar_description_frontmatter() {
        let root = temp_root("folded-description");
        write_skill(
            &root,
            "review",
            "---\nname: review\ndescription: >-\n  Review the changed code\n  and call out risky behavior.\nallowed-tools: Read\n---\nSteps…",
        );

        let locations = vec![user_location(SkillSource::Shared, &root)];
        let catalog = scan_skills(&locations);
        let review = catalog.skills.iter().find(|s| s.name == "review").unwrap();
        assert_eq!(
            review.description,
            "Review the changed code and call out risky behavior."
        );
        assert_eq!(review.allowed_tools.as_deref(), Some("Read"));
        assert_eq!(review.body, "Steps…");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copies_across_roots_group_into_one_entry() {
        let codex_root = temp_root("group-codex");
        let cursor_root = temp_root("group-cursor");
        let opencode_root = temp_root("group-opencode");
        // The installer-drops-it-everywhere layout: same skill, three roots.
        for root in [&codex_root, &cursor_root, &opencode_root] {
            write_skill(
                root,
                "agents-sdk",
                "---\nname: agents-sdk\ndescription: Build AI agents\n---\nX",
            );
        }
        let locations = vec![
            user_location(SkillSource::Provider(ProviderKind::Codex), &codex_root),
            user_location(SkillSource::Provider(ProviderKind::Cursor), &cursor_root),
            user_location(
                SkillSource::Provider(ProviderKind::OpenCode),
                &opencode_root,
            ),
        ];
        let catalog = scan_skills(&locations);
        assert_eq!(catalog.skills.len(), 1, "one row, not one per root");

        let skill = &catalog.skills[0];
        assert_eq!(skill.installs.len(), 3);
        assert_eq!(
            skill.primary().source,
            SkillSource::Provider(ProviderKind::Codex)
        );
        assert_eq!(skill.sources_label(), "Codex · Cursor · OpenCode");
        assert_eq!(skill.duplicates, 0, "grouped copies are not duplicates");

        // A disabled copy next to live ones keeps the skill enabled.
        set_skill_enabled(&cursor_root.join("agents-sdk"), false).unwrap();
        let catalog = scan_skills(&locations);
        assert!(catalog.skills[0].enabled);
        assert_eq!(catalog.skills[0].installs.len(), 3);

        for root in [&codex_root, &cursor_root, &opencode_root] {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn scan_orders_user_before_projects_and_counts_cross_scope_duplicates() {
        let user_root = temp_root("order-user");
        let project_root = temp_root("order-project");
        write_skill(&user_root, "zeta", "---\nname: zeta\n---\nX");
        write_skill(&user_root, "review", "---\nname: review\n---\nX");
        write_skill(&project_root, "review", "---\nname: review\n---\nY");

        let locations = vec![
            user_location(SkillSource::Shared, &user_root),
            SkillLocation {
                source: SkillSource::Provider(ProviderKind::Claude),
                scope: SkillScope::Project,
                root: project_root.clone(),
                project: Some("waku".into()),
            },
        ];
        let catalog = scan_skills(&locations);
        let names: Vec<(&str, Option<&str>)> = catalog
            .skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.project.as_deref()))
            .collect();
        // User scope leads, alphabetical inside each group.
        assert_eq!(
            names,
            vec![("review", None), ("zeta", None), ("review", Some("waku"))]
        );
        // A user-scope and a project-scope copy stay separate rows — the
        // project one shadows at invocation — and both carry the note.
        assert!(
            catalog
                .skills
                .iter()
                .filter(|skill| skill.name == "review")
                .all(|skill| skill.duplicates == 1 && skill.installs.len() == 1)
        );
        let zeta = catalog.skills.iter().find(|s| s.name == "zeta").unwrap();
        assert_eq!(zeta.duplicates, 0);
        let _ = std::fs::remove_dir_all(&user_root);
        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn enable_round_trips_and_never_destroys_a_live_skill_file() {
        let root = temp_root("toggle");
        write_skill(&root, "deploy", "---\nname: deploy\n---\nX");
        let dir = root.join("deploy");

        set_skill_enabled(&dir, false).unwrap();
        assert!(!dir.join(SKILL_FILE).exists());
        assert!(dir.join(DISABLED_SKILL_FILE).is_file());
        // Idempotent both ways.
        set_skill_enabled(&dir, false).unwrap();
        set_skill_enabled(&dir, true).unwrap();
        assert!(dir.join(SKILL_FILE).is_file());
        assert!(!dir.join(DISABLED_SKILL_FILE).exists());
        // Enabling with neither file present is a quiet no-op.
        set_skill_enabled(&root.join("missing"), true).unwrap();

        // A live SKILL.md next to a stale .disabled copy survives an enable.
        std::fs::write(dir.join(DISABLED_SKILL_FILE), "stale").unwrap();
        set_skill_enabled(&dir, true).unwrap();
        let contents = std::fs::read_to_string(dir.join(SKILL_FILE)).unwrap();
        assert!(contents.contains("name: deploy"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_ecosystem_root_is_listed() {
        let project_root = std::env::temp_dir().join("waku-skills-project");
        let projects = vec![("waku".to_owned(), project_root.clone())];
        let locations = skill_locations(&projects);
        for expected in [
            ".agents/skills",
            ".claude/skills",
            ".codex/skills",
            ".config/opencode/skills",
            ".cursor/skills",
            ".pi/agent/skills",
            ".config/agents/skills",
        ] {
            assert!(
                locations
                    .iter()
                    .any(|location| location.root.ends_with(expected)),
                "user root missing: {expected}"
            );
        }
        for expected in [
            ".agents/skills",
            ".claude/skills",
            ".codex/skills",
            ".opencode/skills",
            ".cursor/skills",
            ".pi/skills",
        ] {
            let expected = project_root.join(expected);
            assert!(
                locations.iter().any(|location| location.root == expected),
                "project root missing: {}",
                expected.display()
            );
        }
        // User scope leads the scan, so grouped entries prefer user copies.
        assert!(locations[0].scope == SkillScope::User);
        assert_eq!(locations[0].source, SkillSource::Shared);
    }

    #[test]
    fn user_skill_locations_honor_absolute_claude_and_codex_env_overrides() {
        #[cfg(windows)]
        let (home, custom_claude, custom_codex) = (
            PathBuf::from(r"C:\Users\example"),
            PathBuf::from(r"C:\custom\claude-home"),
            PathBuf::from(r"C:\custom\codex-home"),
        );
        #[cfg(not(windows))]
        let (home, custom_claude, custom_codex) = (
            PathBuf::from("/Users/example"),
            PathBuf::from("/opt/custom/claude-home"),
            PathBuf::from("/opt/custom/codex-home"),
        );

        let overridden = user_skill_locations_with(
            Some(&home),
            Some(custom_claude.as_os_str()),
            Some(custom_codex.as_os_str()),
            None,
        );
        let root_for = |locations: &[SkillLocation], provider: ProviderKind| {
            locations
                .iter()
                .find(|location| location.source == SkillSource::Provider(provider))
                .map(|location| location.root.clone())
        };
        assert_eq!(
            root_for(&overridden, ProviderKind::Claude),
            Some(custom_claude.join("skills"))
        );
        assert_eq!(
            root_for(&overridden, ProviderKind::Codex),
            Some(custom_codex.join("skills"))
        );

        // Relative or empty env values fall back to `~/.claude/skills` and
        // `~/.codex/skills`, matching `composer_complete.rs`.
        let fallback = user_skill_locations_with(
            Some(&home),
            Some(std::ffi::OsStr::new("relative/claude")),
            Some(std::ffi::OsStr::new("relative/codex")),
            Some(std::ffi::OsStr::new("relative/agy")),
        );
        assert_eq!(
            root_for(&fallback, ProviderKind::Claude),
            Some(home.join(".claude").join("skills"))
        );
        assert_eq!(
            root_for(&fallback, ProviderKind::Codex),
            Some(home.join(".codex").join("skills"))
        );
        assert_eq!(
            root_for(&fallback, ProviderKind::Antigravity),
            Some(home.join(".gemini/config/skills"))
        );
    }

    #[test]
    fn antigravity_env_and_account_dirs_register_isolated_skill_roots() {
        let root = temp_root("antigravity-isolated");
        let env_sidecar = root.join("agy-env").join("sidecar_data");
        let account_dir = root.join("agy-work");
        let claude_account = root.join("claude-work");

        let env_locations = user_skill_locations_with(
            Some(&root.join("home")),
            None,
            None,
            Some(env_sidecar.as_os_str()),
        );
        let env_agy_roots: Vec<PathBuf> = env_locations
            .iter()
            .filter(|loc| loc.source == SkillSource::Provider(ProviderKind::Antigravity))
            .map(|loc| loc.root.clone())
            .collect();
        assert!(env_agy_roots.contains(&root.join("agy-env").join("config/skills")));
        assert!(env_agy_roots.contains(&root.join("agy-env").join("skills")));
        assert!(env_agy_roots.contains(&root.join("home/.gemini/config/skills")));
        assert!(env_agy_roots.contains(&root.join("home/.gemini/antigravity-cli/skills")));

        write_skill(
            &account_dir.join("config/skills"),
            "agy-config-skill",
            "---\nname: agy-config-skill\ndescription: Config skill\n---\nBody",
        );
        write_skill(
            &account_dir.join("skills"),
            "agy-cli-skill",
            "---\nname: agy-cli-skill\ndescription: CLI skill\n---\nBody",
        );
        write_skill(
            &claude_account.join("skills"),
            "claude-work-skill",
            "---\nname: claude-work-skill\ndescription: Claude work\n---\nBody",
        );

        let mut locations = Vec::new();
        let accounts = vec![
            (ProviderKind::Antigravity, account_dir.clone()),
            (ProviderKind::Antigravity, account_dir.join("sidecar_data")),
            (ProviderKind::Claude, claude_account.clone()),
        ];
        extend_account_skill_locations(&mut locations, &accounts);
        // `sidecar_data` resolves to the same parent and is deduplicated.
        assert_eq!(locations.len(), 3);

        let catalog = scan_skills(&locations);
        let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["agy-cli-skill", "agy-config-skill", "claude-work-skill"]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn antigravity_underscore_roots_skills_json_and_disable_slash_command_work() {
        let root = temp_root("antigravity-customizations");
        let project = root.join("repo");
        let extra_rel = project.join("tools/shared-skills");
        let inherited_dir = root.join("inherited-pack");
        let inherited_skills = inherited_dir.join("skills");

        write_skill(
            &project.join("_agents/skills"),
            "team-under-agents",
            "---\nname: team-under-agents\ndescription: Under _agents\n---\nBody",
        );
        write_skill(
            &project.join("_agent/skills"),
            "team-under-agent",
            "---\nname: team-under-agent\ndescription: Under _agent\ndisable-slash-command: true\n---\nBody",
        );
        write_skill(
            &extra_rel,
            "manifest-skill",
            "---\nname: manifest-skill\ndescription: From skills.json\n---\nBody",
        );
        write_skill(
            &inherited_skills,
            "inherited-skill",
            "---\nname: inherited-skill\ndescription: From inherited skills.json\n---\nBody",
        );

        std::fs::create_dir_all(inherited_dir.clone()).unwrap();
        let inherited_manifest = inherited_dir.join("skills.json");
        std::fs::write(
            &inherited_manifest,
            serde_json::json!({
                "entries": [{ "path": "skills" }]
            })
            .to_string(),
        )
        .unwrap();

        std::fs::create_dir_all(project.join(".agents")).unwrap();
        std::fs::write(
            project.join(".agents/skills.json"),
            serde_json::json!({
                "inherits": [{ "path": inherited_manifest }],
                "entries": [{ "path": "tools/shared-skills" }]
            })
            .to_string(),
        )
        .unwrap();

        let projects = vec![("repo".to_owned(), project.clone())];
        let locations = skill_locations_for(&projects, &[]);
        let catalog = scan_skills(&locations);
        let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "team-under-agents",
            "team-under-agent",
            "manifest-skill",
            "inherited-skill",
        ] {
            assert!(
                names.contains(&expected),
                "missing Antigravity skill {expected} in {names:?}"
            );
        }

        let disabled_front = parse_skill_frontmatter(
            "---\nname: hidden\ndisable-slash-command: true\n---\nBody",
        );
        assert!(disabled_front.disable_slash_command);
        let enabled_front = parse_skill_frontmatter("---\nname: visible\n---\nBody");
        assert!(!enabled_front.disable_slash_command);

        let _ = std::fs::remove_dir_all(&root);
    }
}

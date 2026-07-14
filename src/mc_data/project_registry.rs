use serde::Deserialize;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// The task system Mission Control should project for a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TaskSource {
    /// No registry unit owns this path. Callers may try a more specific
    /// surface/session path before falling back to legacy Beads discovery.
    Unregistered,
    Beads,
    Linear(LinearTarget),
    /// The registry authoritatively selects Linear, but its coordinates are
    /// incomplete. Callers must show Linear as unavailable rather than
    /// falling back to a stray `.beads` directory.
    LinearUnavailable,
}

/// The registry coordinates that identify one Linear task collection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinearTarget {
    /// Metadata for future write flows; reads require only `project_id`.
    pub team_id: Option<String>,
    pub project_id: String,
    pub labels: Vec<String>,
    /// Registered feature identity used to segment the projected list. This is
    /// display metadata; the feature label above remains the query constraint.
    pub feature_name: Option<String>,
}

/// A sanitized registry failure. It intentionally carries no file contents,
/// paths, or parser detail so it is safe to surface in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    HomeUnavailable,
    Unavailable,
    Malformed,
    InvalidLinearTarget,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::HomeUnavailable => "project registry home is unavailable",
            Self::Unavailable => "project registry is unavailable",
            Self::Malformed => "project registry is malformed",
            Self::InvalidLinearTarget => "Linear registry coordinates are incomplete",
        };
        f.write_str(message)
    }
}

impl std::error::Error for RegistryError {}

/// A parsed registry that can resolve many workspaces without re-reading YAML.
#[derive(Debug, Clone)]
pub struct ProjectRegistry {
    data: RegistryFile,
    home: PathBuf,
}

impl ProjectRegistry {
    /// Load the standard `~/agents/projects.yaml` registry.
    pub fn load_default() -> Result<Self, RegistryError> {
        let home = dirs::home_dir().ok_or(RegistryError::HomeUnavailable)?;
        Self::load_with_home(home.join("agents/projects.yaml"), home)
    }

    /// Load a registry with an explicit home directory. This is useful for
    /// deterministic callers and tests that use portable `~/...` paths.
    pub fn load_with_home(
        path: impl AsRef<Path>,
        home: impl Into<PathBuf>,
    ) -> Result<Self, RegistryError> {
        let yaml = fs::read_to_string(path).map_err(|_| RegistryError::Unavailable)?;
        let data = serde_yaml::from_str(&yaml).map_err(|_| RegistryError::Malformed)?;
        Ok(Self {
            data,
            home: home.into(),
        })
    }

    /// Resolve the effective task source for a workspace path.
    ///
    /// Paths are matched by components, so `/repo/foo` cannot accidentally
    /// match `/repo/foobar`. A feature is preferred when it is at least as
    /// specific as a project binding, matching `projects.py here` behavior.
    pub fn resolve(&self, workspace_path: impl AsRef<Path>) -> TaskSource {
        let workspace = normalize_path(workspace_path.as_ref());
        let project = self.best_project(&workspace);
        let feature = self.best_feature(&workspace);

        if let Some(feature) = feature.filter(|feature| {
            project
                .as_ref()
                .is_none_or(|project| feature.specificity >= project.specificity)
        }) {
            return self.resolve_feature(feature);
        }

        self.resolve_project(project)
    }

    /// Resolve the registered unit a workspace says it is about. Feature names
    /// are more specific than project/platform names; title evidence precedes
    /// description evidence. Ambiguous text intentionally returns
    /// `Unregistered` so callers can fall through to authoritative path
    /// evidence rather than guessing.
    pub fn resolve_workspace_identity(&self, title: &str, description: Option<&str>) -> TaskSource {
        match self.feature_identity_match(title, description) {
            IdentityMatch::One(matched) => return self.resolve_feature(matched),
            IdentityMatch::Ambiguous => return TaskSource::Unregistered,
            IdentityMatch::None => {}
        }

        match self.project_identity_match(title, description) {
            IdentityMatch::One(IdentityUnit::Project(project)) => {
                self.resolve_project(Some(ProjectMatch {
                    project,
                    specificity: 0,
                }))
            }
            IdentityMatch::One(IdentityUnit::Platform(platform)) => self.resolve_platform(platform),
            IdentityMatch::None | IdentityMatch::Ambiguous => TaskSource::Unregistered,
        }
    }

    fn feature_identity_match<'a>(
        &'a self,
        title: &str,
        description: Option<&str>,
    ) -> IdentityMatch<FeatureMatch<'a>> {
        let title_matches = self.features_mentioned_in(title);
        if !title_matches.is_empty() {
            return disambiguate_features(title_matches, title, description);
        }

        let description_matches = description
            .map(|text| self.features_mentioned_in(text))
            .unwrap_or_default();
        disambiguate_features(description_matches, title, description)
    }

    fn features_mentioned_in<'a>(&'a self, text: &str) -> Vec<FeatureMatch<'a>> {
        self.data
            .platforms
            .iter()
            .flat_map(|platform| {
                platform.features.iter().filter_map(move |feature| {
                    feature.name.as_deref().and_then(|name| {
                        identity_mentions_unit(text, name).then(|| FeatureMatch {
                            platform,
                            feature,
                            specificity: name_tokens(name).len(),
                        })
                    })
                })
            })
            .collect()
    }

    fn project_identity_match<'a>(
        &'a self,
        title: &str,
        description: Option<&str>,
    ) -> IdentityMatch<IdentityUnit<'a>> {
        let title_matches = self.units_mentioned_in(title);
        if !title_matches.is_empty() {
            return unique_identity_unit(title_matches);
        }

        let description_matches = description
            .map(|text| self.units_mentioned_in(text))
            .unwrap_or_default();
        unique_identity_unit(description_matches)
    }

    fn units_mentioned_in<'a>(&'a self, text: &str) -> Vec<IdentityUnit<'a>> {
        let mut units = Vec::new();
        for project in &self.data.projects {
            if let Some(name) = project.project.as_deref()
                && identity_mentions_unit(text, name)
            {
                units.push((IdentityUnit::Project(project), name_tokens(name).len()));
            }
        }
        for platform in &self.data.platforms {
            let Some(name) = platform.name.as_deref() else {
                continue;
            };
            if !identity_mentions_unit(text, name) {
                continue;
            }
            // A same-name project is the project facet of this logical unit;
            // resolving it preserves explicit project-tracker precedence and
            // same-name platform inheritance.
            if self.data.projects.iter().any(|project| {
                project
                    .project
                    .as_deref()
                    .is_some_and(|project_name| unit_name_key(project_name) == unit_name_key(name))
            }) {
                continue;
            }
            units.push((IdentityUnit::Platform(platform), name_tokens(name).len()));
        }
        let Some(max_specificity) = units.iter().map(|(_, specificity)| *specificity).max() else {
            return Vec::new();
        };
        units
            .into_iter()
            .filter_map(|(unit, specificity)| (specificity == max_specificity).then_some(unit))
            .collect()
    }

    fn best_project<'a>(&'a self, workspace: &Path) -> Option<ProjectMatch<'a>> {
        let mut candidates = Vec::new();

        for project in &self.data.projects {
            for binding in project.bindings() {
                let repo = normalize_path(&expand_home(binding.path, &self.home));
                let Some(relative) = workspace.strip_prefix(&repo).ok() else {
                    continue;
                };
                let Some(specificity) = matching_root_specificity(relative, binding.roots) else {
                    continue;
                };
                candidates.push(ProjectMatch {
                    project,
                    specificity,
                });
            }
        }

        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.specificity));
        let best = candidates.first()?;
        let tied_names: HashSet<_> = candidates
            .iter()
            .take_while(|candidate| candidate.specificity == best.specificity)
            .map(|candidate| candidate.project.project.as_deref())
            .collect();
        (tied_names.len() == 1).then_some(*best)
    }

    fn best_feature<'a>(&'a self, workspace: &Path) -> Option<FeatureMatch<'a>> {
        let mut best = None;

        for platform in &self.data.platforms {
            for feature in &platform.features {
                let Some(repo_path) = feature.repo.as_deref() else {
                    continue;
                };
                let repo = normalize_path(&expand_home(repo_path, &self.home));
                let Some(relative) = workspace.strip_prefix(&repo).ok() else {
                    continue;
                };
                let Some(specificity) = matching_root_specificity(relative, &feature.roots) else {
                    continue;
                };
                if best
                    .as_ref()
                    .is_none_or(|current: &FeatureMatch<'_>| specificity > current.specificity)
                {
                    best = Some(FeatureMatch {
                        platform,
                        feature,
                        specificity,
                    });
                }
            }
        }

        best
    }

    fn resolve_feature(&self, matched: FeatureMatch<'_>) -> TaskSource {
        let tracker = matched
            .feature
            .tracker
            .as_deref()
            .or(matched.platform.tracker.as_deref())
            .unwrap_or("beads");
        if tracker != "linear" {
            return TaskSource::Beads;
        }

        let linear = RawLinear::merged(
            matched.platform.linear.as_ref(),
            matched.feature.linear.as_ref(),
        );
        let Ok(mut target) = LinearTarget::try_from(linear) else {
            return TaskSource::LinearUnavailable;
        };
        if let Some(feature_name) = non_empty(matched.feature.name.as_deref()) {
            push_unique(&mut target.labels, feature_name);
            target.feature_name = Some(feature_name.to_owned());
        }
        target.labels.sort();
        TaskSource::Linear(target)
    }

    fn resolve_platform(&self, platform: &PlatformEntry) -> TaskSource {
        if platform.tracker.as_deref().unwrap_or("beads") != "linear" {
            return TaskSource::Beads;
        }
        LinearTarget::try_from(platform.linear.clone().unwrap_or_default())
            .map(TaskSource::Linear)
            .unwrap_or(TaskSource::LinearUnavailable)
    }

    fn resolve_project(&self, matched: Option<ProjectMatch<'_>>) -> TaskSource {
        let Some(matched) = matched else {
            return TaskSource::Unregistered;
        };

        let same_name_platform = matched.project.project.as_deref().and_then(|name| {
            self.data
                .platforms
                .iter()
                .find(|platform| platform.name.as_deref() == Some(name))
        });

        // Match projects.py: an explicit project tracker owns the project
        // facet. A same-name platform supplies tracking only when the project
        // leaves it unspecified.
        let (tracker, linear) = if let Some(tracker) = matched.project.tracker.as_deref() {
            (tracker, matched.project.linear.clone())
        } else if let Some(platform) = same_name_platform {
            (
                platform.tracker.as_deref().unwrap_or("beads"),
                platform.linear.clone(),
            )
        } else {
            ("beads", None)
        };
        if tracker != "linear" {
            return TaskSource::Beads;
        }

        LinearTarget::try_from(linear.unwrap_or_default())
            .map(TaskSource::Linear)
            .unwrap_or(TaskSource::LinearUnavailable)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct RegistryFile {
    projects: Vec<ProjectEntry>,
    platforms: Vec<PlatformEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ProjectEntry {
    project: Option<String>,
    path: Option<String>,
    roots: Vec<String>,
    repos: Vec<RepoBinding>,
    tracker: Option<String>,
    linear: Option<RawLinear>,
}

impl ProjectEntry {
    fn bindings(&self) -> Vec<NormalizedBinding<'_>> {
        if !self.repos.is_empty() {
            return self
                .repos
                .iter()
                .map(|binding| match binding {
                    RepoBinding::Path(path) => NormalizedBinding { path, roots: &[] },
                    RepoBinding::Detailed(binding) => NormalizedBinding {
                        path: &binding.path,
                        roots: &binding.roots,
                    },
                })
                .collect();
        }

        self.path
            .as_deref()
            .map(|path| NormalizedBinding {
                path,
                roots: &self.roots,
            })
            .into_iter()
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RepoBinding {
    Path(String),
    Detailed(DetailedBinding),
}

#[derive(Debug, Clone, Deserialize)]
struct DetailedBinding {
    path: String,
    #[serde(default)]
    roots: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct NormalizedBinding<'a> {
    path: &'a str,
    roots: &'a [String],
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct PlatformEntry {
    name: Option<String>,
    tracker: Option<String>,
    linear: Option<RawLinear>,
    features: Vec<FeatureEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct FeatureEntry {
    name: Option<String>,
    repo: Option<String>,
    roots: Vec<String>,
    tracker: Option<String>,
    linear: Option<RawLinear>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct RawLinear {
    team_id: Option<String>,
    project_id: Option<String>,
    labels: Option<Vec<String>>,
}

impl RawLinear {
    /// Merge a base and override block the same way `projects.py` merges
    /// platform and feature tracking: explicitly supplied override fields win.
    fn merged(base: Option<&Self>, override_: Option<&Self>) -> Self {
        let mut merged = base.cloned().unwrap_or_default();
        if let Some(override_) = override_ {
            if override_.team_id.is_some() {
                merged.team_id.clone_from(&override_.team_id);
            }
            if override_.project_id.is_some() {
                merged.project_id.clone_from(&override_.project_id);
            }
            if override_.labels.is_some() {
                merged.labels.clone_from(&override_.labels);
            }
        }
        merged
    }
}

impl TryFrom<RawLinear> for LinearTarget {
    type Error = RegistryError;

    fn try_from(value: RawLinear) -> Result<Self, Self::Error> {
        let team_id = value.team_id.and_then(non_empty_owned);
        let project_id = value
            .project_id
            .and_then(non_empty_owned)
            .ok_or(RegistryError::InvalidLinearTarget)?;
        let mut labels = Vec::new();
        for label in value.labels.unwrap_or_default() {
            if let Some(label) = non_empty_owned(label) {
                push_unique(&mut labels, &label);
            }
        }
        labels.sort();
        Ok(Self {
            team_id,
            project_id,
            labels,
            feature_name: None,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ProjectMatch<'a> {
    project: &'a ProjectEntry,
    specificity: usize,
}

#[derive(Debug, Clone, Copy)]
struct FeatureMatch<'a> {
    platform: &'a PlatformEntry,
    feature: &'a FeatureEntry,
    specificity: usize,
}

#[derive(Debug, Clone, Copy)]
enum IdentityUnit<'a> {
    Project(&'a ProjectEntry),
    Platform(&'a PlatformEntry),
}

enum IdentityMatch<T> {
    None,
    One(T),
    Ambiguous,
}

fn disambiguate_features<'a>(
    mut matches: Vec<FeatureMatch<'a>>,
    title: &str,
    description: Option<&str>,
) -> IdentityMatch<FeatureMatch<'a>> {
    if matches.is_empty() {
        return IdentityMatch::None;
    }
    let max_specificity = matches
        .iter()
        .map(|matched| matched.specificity)
        .max()
        .unwrap_or_default();
    matches.retain(|matched| matched.specificity == max_specificity);
    if matches.len() == 1 {
        return IdentityMatch::One(matches[0]);
    }

    let platform_matches: Vec<_> = matches
        .iter()
        .copied()
        .filter(|matched| {
            matched.platform.name.as_deref().is_some_and(|name| {
                identity_mentions_unit(title, name)
                    || description.is_some_and(|text| identity_mentions_unit(text, name))
            })
        })
        .collect();
    if !platform_matches.is_empty() {
        matches = platform_matches;
    }
    if matches.len() == 1 {
        IdentityMatch::One(matches[0])
    } else {
        IdentityMatch::Ambiguous
    }
}

fn unique_identity_unit(matches: Vec<IdentityUnit<'_>>) -> IdentityMatch<IdentityUnit<'_>> {
    match matches.as_slice() {
        [] => IdentityMatch::None,
        [matched] => IdentityMatch::One(*matched),
        _ => IdentityMatch::Ambiguous,
    }
}

fn matching_root_specificity(relative: &Path, roots: &[String]) -> Option<usize> {
    if roots.is_empty() {
        return Some(0);
    }

    roots
        .iter()
        .filter_map(|root| {
            let root = Path::new(root.trim_matches('/'));
            (!root.as_os_str().is_empty() && (relative == root || relative.starts_with(root)))
                .then(|| root.components().count())
        })
        .max()
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    path.strip_prefix("~/")
        .map_or_else(|| PathBuf::from(path), |suffix| home.join(suffix))
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn non_empty_owned(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|candidate| candidate == value) {
        values.push(value.to_owned());
    }
}

fn unit_name_key(value: &str) -> String {
    let mut tokens: Vec<String> = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if let Some(last) = tokens.last_mut()
        && last.len() > 3
        && last.ends_with('s')
        && !last.ends_with("ss")
    {
        last.pop();
    }
    tokens.join("-")
}

fn identity_mentions_unit(identity: &str, unit_name: &str) -> bool {
    let identity_tokens = name_tokens(identity);
    let unit_tokens = name_tokens(unit_name);
    if unit_tokens.is_empty() || unit_tokens.len() > identity_tokens.len() {
        return false;
    }
    if identity_tokens
        .windows(unit_tokens.len())
        .any(|window| window == unit_tokens)
    {
        return true;
    }

    let singular_identity: Vec<_> = identity_tokens
        .iter()
        .map(|token| singular_name_token(token))
        .collect();
    let singular_unit: Vec<_> = unit_tokens
        .iter()
        .map(|token| singular_name_token(token))
        .collect();
    singular_identity
        .windows(singular_unit.len())
        .any(|window| window == singular_unit)
}

fn name_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn singular_name_token(token: &String) -> String {
    let mut token = token.clone();
    if token.len() > 3 && token.ends_with('s') && !token.ends_with("ss") {
        token.pop();
    }
    token
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry(yaml: &str) -> (TempDir, ProjectRegistry) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("projects.yaml");
        fs::write(&path, yaml).unwrap();
        let registry = ProjectRegistry::load_with_home(&path, temp.path()).unwrap();
        (temp, registry)
    }

    #[test]
    fn ordinary_registered_project_defaults_to_beads() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: mission-control
    path: ~/Tools/mission-control
"#,
        );
        let workspace = temp.path().join("Tools/mission-control/src");

        assert_eq!(registry.resolve(workspace), TaskSource::Beads);
    }

    #[test]
    fn explicit_project_tracker_overrides_same_name_platform() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: olympus
    path: ~/Projects/olympus
    tracker: beads
platforms:
  - name: olympus
    tracker: linear
    linear:
      team_id: team-1
      project_id: project-1
      labels: [platform]
"#,
        );
        let workspace = temp.path().join("Projects/olympus");

        assert_eq!(registry.resolve(workspace), TaskSource::Beads);
    }

    #[test]
    fn same_name_platform_supplies_linear_when_project_tracker_is_unspecified() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: olympus
    path: ~/Projects/olympus
platforms:
  - name: olympus
    tracker: linear
    linear:
      team_id: team-1
      project_id: project-1
      labels: [platform]
"#,
        );
        let workspace = temp.path().join("Projects/olympus");

        assert_eq!(
            registry.resolve(workspace),
            TaskSource::Linear(LinearTarget {
                team_id: Some("team-1".into()),
                project_id: "project-1".into(),
                labels: vec!["platform".into()],
                feature_name: None,
            })
        );
    }

    #[test]
    fn feature_inherits_linear_and_adds_its_name_as_a_label() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: olympus
    path: ~/Projects/olympus
platforms:
  - name: olympus
    tracker: linear
    linear:
      team_id: team-1
      project_id: project-1
      labels: [configured]
    features:
      - name: group-grader
        repo: ~/Projects/olympus
        roots: [olympus/projects/minos/graders]
"#,
        );
        let workspace = temp
            .path()
            .join("Projects/olympus/olympus/projects/minos/graders/tests");

        assert_eq!(
            registry.resolve(workspace),
            TaskSource::Linear(LinearTarget {
                team_id: Some("team-1".into()),
                project_id: "project-1".into(),
                labels: vec!["configured".into(), "group-grader".into()],
                feature_name: Some("group-grader".into()),
            })
        );
    }

    #[test]
    fn unique_plural_workspace_name_resolves_the_linear_feature() {
        let (_temp, registry) = registry(
            r#"
platforms:
  - name: olympus
    tracker: linear
    linear:
      project_id: project-1
    features:
      - name: group-grader
        repo: ~/Projects/olympus
        roots: [olympus/projects/minos/graders]
"#,
        );

        assert_eq!(
            registry.resolve_workspace_identity("group-graders", None),
            TaskSource::Linear(LinearTarget {
                team_id: None,
                project_id: "project-1".to_string(),
                labels: vec!["group-grader".to_string()],
                feature_name: Some("group-grader".to_string()),
            })
        );
        assert_eq!(
            registry.resolve_workspace_identity("unrelated", None),
            TaskSource::Unregistered
        );
    }

    #[test]
    fn title_or_description_resolves_the_registered_feature_identity() {
        let (_temp, registry) = registry(
            r#"
projects:
  - project: agents
    path: ~/agents/blin-agents
platforms:
  - name: olympus
    tracker: linear
    linear:
      project_id: project-1
    features:
      - name: group-grader
        repo: ~/Projects/olympus
        roots: [olympus/projects/minos/graders]
"#,
        );

        for source in [
            registry.resolve_workspace_identity("group-graders", None),
            registry.resolve_workspace_identity(
                "evaluation work",
                Some("Build the group grader feature under the Olympus platform"),
            ),
        ] {
            assert!(matches!(
                source,
                TaskSource::Linear(LinearTarget { labels, .. })
                    if labels == ["group-grader".to_string()]
            ));
        }
    }

    #[test]
    fn platform_context_disambiguates_duplicate_feature_names() {
        let (_temp, registry) = registry(
            r#"
platforms:
  - name: olympus
    tracker: linear
    linear:
      project_id: olympus-project
    features:
      - name: grader
  - name: academy
    tracker: linear
    linear:
      project_id: academy-project
    features:
      - name: grader
"#,
        );

        assert_eq!(
            registry.resolve_workspace_identity("grader", None),
            TaskSource::Unregistered
        );
        assert!(matches!(
            registry.resolve_workspace_identity(
                "grader",
                Some("This is the Olympus platform feature")
            ),
            TaskSource::Linear(LinearTarget { project_id, .. })
                if project_id == "olympus-project"
        ));
    }

    #[test]
    fn ambiguous_feature_identity_does_not_guess() {
        let (_temp, registry) = registry(
            r#"
platforms:
  - name: olympus
    tracker: linear
    linear:
      project_id: project-1
    features:
      - name: group-grader
      - name: trace-viewer
"#,
        );

        assert_eq!(
            registry.resolve_workspace_identity(
                "group-grader trace-viewer",
                Some("Two possible Olympus features")
            ),
            TaskSource::Unregistered
        );
    }

    #[test]
    fn longest_registered_unit_name_is_the_identity_match() {
        let (_temp, registry) = registry(
            r#"
projects:
  - project: olympus
    path: ~/Projects/olympus
  - project: wiki-olympus
    path: ~/Projects/wiki-olympus
    tracker: beads
platforms:
  - name: olympus
    tracker: linear
    linear:
      project_id: project-1
    features:
      - name: grader
      - name: group-grader
"#,
        );

        assert!(matches!(
            registry.resolve_workspace_identity("group graders", None),
            TaskSource::Linear(LinearTarget { feature_name, .. })
                if feature_name.as_deref() == Some("group-grader")
        ));
        assert_eq!(
            registry.resolve_workspace_identity("wiki olympus", None),
            TaskSource::Beads
        );
    }

    #[test]
    fn most_specific_project_root_wins() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: monorepo
    path: ~/Projects/repo
  - project: nested
    path: ~/Projects/repo
    roots: [services/nested]
    tracker: linear
    linear:
      team_id: team-2
      project_id: project-2
"#,
        );
        let workspace = temp.path().join("Projects/repo/services/nested/src");

        assert_eq!(
            registry.resolve(workspace),
            TaskSource::Linear(LinearTarget {
                team_id: Some("team-2".into()),
                project_id: "project-2".into(),
                labels: Vec::new(),
                feature_name: None,
            })
        );
    }

    #[test]
    fn supports_string_and_detailed_repo_bindings() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: shared
    repos:
      - ~/Projects/first
      - path: ~/Projects/second
        roots: [owned]
    tracker: linear
    linear:
      team_id: team-3
      project_id: project-3
"#,
        );

        assert!(matches!(
            registry.resolve(temp.path().join("Projects/first")),
            TaskSource::Linear(_)
        ));
        assert!(matches!(
            registry.resolve(temp.path().join("Projects/second/owned/src")),
            TaskSource::Linear(_)
        ));
        assert_eq!(
            registry.resolve(temp.path().join("Projects/second/unowned")),
            TaskSource::Unregistered
        );
    }

    #[test]
    fn path_matching_uses_components_not_string_prefixes() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: foo
    path: ~/Projects/foo
    tracker: linear
    linear:
      team_id: team
      project_id: project
"#,
        );

        assert_eq!(
            registry.resolve(temp.path().join("Projects/foobar")),
            TaskSource::Unregistered
        );
    }

    #[test]
    fn incomplete_linear_coordinates_preserve_authoritative_linear_selection() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: broken
    path: ~/Projects/broken
    tracker: linear
    linear:
      team_id: secret-team-value
"#,
        );

        assert_eq!(
            registry.resolve(temp.path().join("Projects/broken")),
            TaskSource::LinearUnavailable
        );
    }

    #[test]
    fn linear_reads_require_project_id_but_not_team_id() {
        let (temp, registry) = registry(
            r#"
projects:
  - project: read-only
    path: ~/Projects/read-only
    tracker: linear
    linear:
      project_id: project-only
"#,
        );

        assert_eq!(
            registry.resolve(temp.path().join("Projects/read-only")),
            TaskSource::Linear(LinearTarget {
                team_id: None,
                project_id: "project-only".to_string(),
                labels: vec![],
                feature_name: None,
            })
        );
    }

    #[test]
    fn missing_and_malformed_registry_errors_are_sanitized() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            ProjectRegistry::load_with_home(temp.path().join("missing.yaml"), temp.path())
                .unwrap_err(),
            RegistryError::Unavailable
        );

        let path = temp.path().join("projects.yaml");
        fs::write(&path, "projects: [definitely: malformed").unwrap();
        let error = ProjectRegistry::load_with_home(path, temp.path()).unwrap_err();
        assert_eq!(error, RegistryError::Malformed);
        assert_eq!(error.to_string(), "project registry is malformed");
    }
}

//! Venue catalog, secrets, and filesystem paths.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const DEFAULT_BUNDLES: &[&str] = &["security", "ml", "se"];
const BUNDLED_VENUES: &[(&str, &str)] = &[
    ("security", include_str!("../venues/security.yaml")),
    ("ml", include_str!("../venues/ml.yaml")),
    ("se", include_str!("../venues/se.yaml")),
    ("hci", include_str!("../venues/hci.yaml")),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Venue {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub dblp_stream: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub rank: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Venue {
    /// Case-insensitive match against the venue id or any alias.
    pub fn matches(&self, needle: &str) -> bool {
        self.id.eq_ignore_ascii_case(needle)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(needle))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Defaults {
    #[serde(default = "default_min_year")]
    pub min_year: i32,
}

fn default_min_year() -> i32 {
    2000
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            min_year: default_min_year(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default = "default_bundles")]
    pub bundles: Vec<String>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub venues: Vec<Venue>,
}

pub type RankSortOrder = Vec<Vec<String>>;

#[derive(Debug, Serialize, Deserialize, Default)]
struct ConfigFile {
    bundles: Option<Vec<String>>,
    defaults: Option<Defaults>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    venues: Vec<Venue>,
}

impl ConfigFile {
    fn from_yaml(yaml: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(yaml)?)
    }
}

#[derive(Debug, Deserialize)]
struct BundleFile {
    #[serde(default)]
    venues: Vec<Venue>,
}

fn default_bundles() -> Vec<String> {
    DEFAULT_BUNDLES
        .iter()
        .map(|bundle| (*bundle).to_string())
        .collect()
}

impl Config {
    /// Load the default bundle set.
    pub fn defaults() -> Result<Self> {
        Self::from_file(ConfigFile::default(), None)
    }

    pub fn from_yaml(yaml: &str) -> Result<Self> {
        Self::from_file(ConfigFile::from_yaml(yaml)?, None)
    }

    pub fn default_user_config_yaml() -> Result<String> {
        Ok(serde_yaml::to_string(&ConfigFile {
            bundles: Some(default_bundles()),
            defaults: Some(Defaults::default()),
            venues: Vec::new(),
        })?)
    }

    pub fn load_with_bundles(
        user_override: Option<&Path>,
        bundle_override: Option<&[String]>,
    ) -> Result<Self> {
        let file = match user_override {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => ConfigFile::from_yaml(&text)?,
                Err(e) if e.kind() == ErrorKind::NotFound => ConfigFile::default(),
                Err(e) => return Err(e.into()),
            },
            None => ConfigFile::default(),
        };
        Self::from_file(file, bundle_override)
    }

    fn from_file(file: ConfigFile, bundle_override: Option<&[String]>) -> Result<Self> {
        let bundles = bundle_override
            .map(|bundles| bundles.to_vec())
            .or(file.bundles)
            .unwrap_or_else(default_bundles);
        let mut cfg = Self {
            bundles: bundles.clone(),
            defaults: file.defaults.unwrap_or_default(),
            venues: Vec::new(),
        };
        for bundle in &bundles {
            cfg.merge_bundle(bundle)?;
        }
        cfg.merge_venues(file.venues);
        Ok(cfg)
    }

    fn merge_bundle(&mut self, bundle: &str) -> Result<()> {
        let Some((_, yaml)) = BUNDLED_VENUES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(bundle))
        else {
            return Err(Error::Config(format!("unknown bundle: {bundle}")));
        };
        let bundle: BundleFile = serde_yaml::from_str(yaml)?;
        self.merge_venues(bundle.venues);
        Ok(())
    }

    fn merge_venues(&mut self, venues: Vec<Venue>) {
        for venue in venues {
            self.upsert_venue(venue);
        }
    }

    fn upsert_venue(&mut self, venue: Venue) {
        match self
            .venues
            .iter_mut()
            .find(|v| v.id.eq_ignore_ascii_case(&venue.id))
        {
            Some(existing) => *existing = venue,
            None => self.venues.push(venue),
        }
    }

    /// Resolve a venue by id or alias.
    pub fn venue(&self, needle: &str) -> Option<&Venue> {
        self.venues.iter().find(|v| v.matches(needle))
    }

    /// Resolve venue selectors (id or alias) to canonical ids.
    /// Unknown selectors produce an error listing them.
    pub fn resolve_venues(&self, selectors: &[String]) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        let mut unknown = Vec::new();
        for sel in selectors {
            match self.venue(sel) {
                Some(v) => {
                    if seen.insert(v.id.clone()) {
                        ids.push(v.id.clone());
                    }
                }
                None => unknown.push(sel.clone()),
            }
        }
        if !unknown.is_empty() {
            return Err(Error::Config(format!(
                "unknown venue(s): {}",
                unknown.join(", ")
            )));
        }
        Ok(ids)
    }

    /// All configured venue ids in catalog order.
    pub fn all_venue_ids(&self) -> Vec<String> {
        self.venues.iter().map(|v| v.id.clone()).collect()
    }

    /// Venue ids matching the given rank labels (case-insensitive).
    pub fn venues_by_rank(&self, ranks: &[String]) -> Vec<String> {
        if ranks.is_empty() {
            return Vec::new();
        }
        self.venues
            .iter()
            .filter(|v| {
                venue_rank(v).is_some_and(|r| ranks.iter().any(|q| q.eq_ignore_ascii_case(r)))
            })
            .map(|v| v.id.clone())
            .collect()
    }

    /// Venue ids carrying any of the given tags (case-insensitive).
    pub fn venues_by_tag(&self, tags: &[String]) -> Vec<String> {
        if tags.is_empty() {
            return Vec::new();
        }
        self.venues
            .iter()
            .filter(|v| {
                v.tags
                    .iter()
                    .any(|t| tags.iter().any(|q| q.eq_ignore_ascii_case(t)))
            })
            .map(|v| v.id.clone())
            .collect()
    }

    /// Venue ids grouped by rank, ordered for rank-based result sorting.
    pub fn rank_sort_order(&self) -> RankSortOrder {
        struct RankGroup {
            label: String,
            sort_key: u8,
        }

        let mut groups: Vec<RankGroup> = Vec::new();
        for venue in &self.venues {
            let Some(rank) = venue_rank(venue).map(str::to_ascii_uppercase) else {
                continue;
            };
            if groups.iter().any(|group| group.label == rank) {
                continue;
            }
            groups.push(RankGroup {
                sort_key: rank_sort_key(&rank),
                label: rank,
            });
        }
        groups.sort_by_key(|group| group.sort_key);
        groups
            .into_iter()
            .map(|group| self.venues_by_rank(std::slice::from_ref(&group.label)))
            .collect()
    }
}

fn venue_rank(venue: &Venue) -> Option<&str> {
    venue.rank.as_deref().filter(|rank| !rank.is_empty())
}

fn rank_sort_key(rank: &str) -> u8 {
    match rank.to_ascii_uppercase().as_str() {
        "A*" => 0,
        "A" => 1,
        "B" => 2,
        "C" => 3,
        _ => 4,
    }
}

/// API keys, read from the environment (optionally seeded from a `.env` file).
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    pub openalex_api_key: Option<String>,
    pub semantic_scholar_key: Option<String>,
    pub openreview_username: Option<String>,
    pub openreview_password: Option<String>,
}

impl Secrets {
    /// Best-effort load: source `.env` if present, then read known vars.
    pub fn load() -> Self {
        let _ = dotenvy::dotenv();
        Self {
            openalex_api_key: non_empty_env("OPENALEX_API_KEY"),
            semantic_scholar_key: non_empty_env("SEMANTIC_SCHOLAR_S2_KEY"),
            openreview_username: non_empty_env("OPENREVIEW_USERNAME"),
            openreview_password: non_empty_env("OPENREVIEW_PASSWORD"),
        }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Resolved on-disk locations for the database and user config.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "cs-grep")
            .ok_or_else(|| Error::Config("cannot determine home directory".into()))?;
        Ok(Self {
            data_dir: dirs.data_dir().to_path_buf(),
            config_dir: dirs.config_dir().to_path_buf(),
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("papers.db")
    }

    pub fn user_config_path(&self) -> PathBuf {
        self.config_dir.join("config.yaml")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.config_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venue(id: &str, rank: &str, tags: &[&str]) -> Venue {
        Venue {
            id: id.to_string(),
            name: String::new(),
            dblp_stream: format!("conf/{}", id.to_ascii_lowercase()),
            aliases: Vec::new(),
            rank: (!rank.is_empty()).then(|| rank.to_string()),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
        }
    }

    #[test]
    fn bundled_venue_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (bundle, yaml) in BUNDLED_VENUES {
            let bundle_file: BundleFile = serde_yaml::from_str(yaml).unwrap();
            for venue in bundle_file.venues {
                assert!(
                    seen.insert(venue.id.to_ascii_lowercase()),
                    "duplicate bundled venue id {} in {bundle}",
                    venue.id
                );
            }
        }
    }

    #[test]
    fn bundled_tags_match_venue_families() {
        let cfg = Config::from_yaml("bundles: [security, ml, se, hci]\n").unwrap();
        let expected: [(&str, &[&str]); 6] = [
            (
                "security",
                &[
                    "NDSS",
                    "USENIX-SEC",
                    "SP",
                    "CCS",
                    "RAID",
                    "ACSAC",
                    "ESORICS",
                    "AsiaCCS",
                    "EuroSP",
                    "AISec",
                    "SaTML",
                    "SOUPS",
                ],
            ),
            (
                "ml",
                &[
                    "AISec", "SaTML", "NeurIPS", "ICML", "ICLR", "ACL", "EMNLP", "CVPR", "ICCV",
                    "IJCAI", "AAAI", "FAccT",
                ],
            ),
            ("nlp", &["ACL", "EMNLP"]),
            ("cv", &["CVPR", "ICCV"]),
            ("se", &["ICSE", "FSE", "ASE"]),
            ("hci", &["CHI", "HCII", "FAccT", "SOUPS"]),
        ];

        let actual_tags = cfg
            .venues
            .iter()
            .flat_map(|venue| venue.tags.iter().map(String::as_str))
            .collect::<std::collections::BTreeSet<_>>();
        let expected_tags = expected
            .iter()
            .map(|(tag, _)| *tag)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual_tags, expected_tags);

        for (tag, expected_venues) in expected {
            assert_eq!(
                cfg.venues_by_tag(&[tag.into()])
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>(),
                expected_venues
                    .iter()
                    .map(|id| (*id).to_string())
                    .collect::<std::collections::BTreeSet<_>>(),
                "wrong venues for tag {tag}"
            );
        }

        for venue in &cfg.venues {
            assert!(!venue.tags.is_empty(), "{} has no tag", venue.id);
            assert_eq!(
                venue
                    .tags
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                venue.tags.len(),
                "{} has duplicate tags",
                venue.id
            );
        }
    }

    #[test]
    fn lookup_by_alias_is_case_insensitive() {
        let cfg = Config::defaults().unwrap();
        assert_eq!(cfg.venue("oakland").unwrap().id, "SP");
        assert_eq!(cfg.venue("USENIX").unwrap().id, "USENIX-SEC");
        assert_eq!(cfg.venue("Ndss").unwrap().id, "NDSS");
        assert_eq!(cfg.venue("ijcai").unwrap().id, "IJCAI");
        assert!(cfg.venue("nope").is_none());
    }

    #[test]
    fn merge_replaces_existing_and_adds_new() {
        let cfg = Config::from_yaml(
            r#"
defaults:
  min_year: 2015
venues:
  - id: NDSS
    dblp_stream: conf/ndss
    rank: B
    aliases: [ndss]
  - id: MYVENUE
    dblp_stream: conf/myv
    aliases: [myv]
"#,
        )
        .unwrap();
        assert_eq!(cfg.defaults.min_year, 2015);
        assert_eq!(cfg.venue("NDSS").unwrap().rank.as_deref(), Some("B"));
        assert!(cfg.venue("myv").is_some());
    }

    #[test]
    fn config_without_defaults_uses_default_min_year() {
        let cfg = Config::from_yaml(
            r#"
venues:
  - id: MYVENUE
    dblp_stream: conf/myv
"#,
        )
        .unwrap();
        assert_eq!(cfg.defaults.min_year, 2000);
        assert!(cfg.venue("MYVENUE").is_some());
    }

    #[test]
    fn generated_default_config_parses() {
        let yaml = Config::default_user_config_yaml().unwrap();
        assert!(!yaml.contains("venues: []"));
        let cfg = Config::from_yaml(&yaml).unwrap();
        assert_eq!(cfg.bundles, vec!["security", "ml", "se"]);
        assert_eq!(cfg.defaults.min_year, 2000);
        assert!(cfg.venue("NDSS").is_some());
        assert!(cfg.venue("ICSE").is_some());
    }

    #[test]
    fn bundle_selection_limits_bundled_venues() {
        let cfg = Config::from_yaml("bundles: [se]\n").unwrap();
        assert_eq!(cfg.bundles, vec!["se"]);
        assert!(cfg.venue("ICSE").is_some());
        assert!(cfg.venue("NDSS").is_none());
    }

    #[test]
    fn empty_bundles_loads_only_custom_venues() {
        let cfg = Config::from_yaml(
            r#"
bundles: []
venues:
  - id: LOCAL
    dblp_stream: conf/local
"#,
        )
        .unwrap();
        assert!(cfg.venue("LOCAL").is_some());
        assert!(cfg.venue("NDSS").is_none());
    }

    #[test]
    fn unknown_bundle_errors() {
        assert!(Config::from_yaml("bundles: [bogus]\n").is_err());
    }

    #[test]
    fn bundle_override_preserves_user_venues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            r#"
bundles: [security]
venues:
  - id: LOCAL
    dblp_stream: conf/local
"#,
        )
        .unwrap();
        let bundles = vec!["se".to_string()];
        let cfg = Config::load_with_bundles(Some(&path), Some(&bundles)).unwrap();
        assert!(cfg.venue("ICSE").is_some());
        assert!(cfg.venue("NDSS").is_none());
        assert!(cfg.venue("LOCAL").is_some());
    }

    #[test]
    fn bundle_override_keeps_user_override_for_unselected_bundle_venue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            r#"
bundles: [security]
venues:
  - id: NDSS
    dblp_stream: conf/custom-ndss
    rank: custom
"#,
        )
        .unwrap();
        let bundles = vec!["se".to_string()];
        let cfg = Config::load_with_bundles(Some(&path), Some(&bundles)).unwrap();
        let venue = cfg.venue("NDSS").unwrap();
        assert_eq!(venue.dblp_stream, "conf/custom-ndss");
        assert_eq!(venue.rank.as_deref(), Some("custom"));
    }

    #[test]
    fn rank_sort_order_sorts_by_rank() {
        let cfg = Config {
            bundles: Vec::new(),
            defaults: Defaults::default(),
            venues: vec![
                venue("BVENUE", "B", &[]),
                venue("ASTAR1", "a*", &[]),
                venue("AVENUE", "A", &[]),
                venue("ASTAR2", "A*", &[]),
                venue("UNRANKED", "", &[]),
            ],
        };
        let groups = cfg.rank_sort_order();
        assert_eq!(
            groups.first(),
            Some(&vec!["ASTAR1".to_string(), "ASTAR2".to_string()])
        );
        assert_eq!(groups.get(1), Some(&vec!["AVENUE".to_string()]));
        assert_eq!(groups.get(2), Some(&vec!["BVENUE".to_string()]));
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn resolve_venues_reports_unknown() {
        let cfg = Config::defaults().unwrap();
        let ok = cfg
            .resolve_venues(&["ndss".into(), "oakland".into()])
            .unwrap();
        assert_eq!(ok, vec!["NDSS".to_string(), "SP".to_string()]);
        assert!(cfg.resolve_venues(&["bogus".into()]).is_err());
    }
}

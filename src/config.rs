use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub targets: BTreeMap<String, Target>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Project {
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Target {
    pub adapter: String,
    pub device: Option<String>,
    pub url: Option<String>,
    pub command: Option<String>,
    pub build: Option<Build>,
    pub cwd: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Build {
    One(String),
    Many(Vec<String>),
}

impl Config {
    pub fn load(path: &Path) -> Result<(Config, PathBuf), String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        let base = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let root = match cfg.project.root.as_deref() {
            Some(r) => base.join(r),
            None => base,
        };
        Ok((cfg, root))
    }
}

impl Target {
    pub fn cwd(&self, root: &Path) -> PathBuf {
        match self.cwd.as_deref() {
            Some(c) => root.join(c),
            None => root.to_path_buf(),
        }
    }

    pub fn build_command(&self) -> Option<(String, Vec<String>)> {
        match &self.build {
            Some(Build::One(s)) => Some(("/bin/sh".into(), vec!["-lc".into(), s.clone()])),
            Some(Build::Many(v)) if !v.is_empty() => Some((v[0].clone(), v[1..].to_vec())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn single_build_command_uses_a_shell() {
        let cfg: Config = toml::from_str("[targets.ios]\nadapter = \"ios\"\nbuild = \"echo hi\"\n")
            .expect("config");
        let (program, args) = cfg.targets["ios"].build_command().expect("build");
        assert_eq!(program, "/bin/sh");
        assert_eq!(args, vec!["-lc".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn many_build_command_runs_directly() {
        let cfg: Config =
            toml::from_str("[targets.web]\nadapter = \"web\"\nbuild = [\"echo\", \"hi\"]\n")
                .expect("config");
        let (program, args) = cfg.targets["web"].build_command().expect("build");
        assert_eq!(program, "echo");
        assert_eq!(args, vec!["hi".to_string()]);
    }
}

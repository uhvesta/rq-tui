use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub(crate) struct AppPaths {
    pub(crate) data: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) database: PathBuf,
    pub(crate) roots: PathBuf,
    pub(crate) prs: PathBuf,
    pub(crate) exports: PathBuf,
    pub(crate) skills: PathBuf,
}

impl AppPaths {
    pub(crate) fn discover() -> Result<Self> {
        let data = env_path("RQ_TUI_DATA_DIR")
            .or_else(|| dirs::data_local_dir().map(|path| path.join("rq-tui")))
            .context("cannot determine local data directory")?;
        let cache = env_path("RQ_TUI_CACHE_DIR")
            .or_else(|| dirs::cache_dir().map(|path| path.join("rq-tui")))
            .context("cannot determine cache directory")?;
        let database = env_path("RQ_TUI_DATABASE").unwrap_or_else(|| data.join("rq-tui.db"));

        let paths = Self {
            database,
            roots: data.join("roots"),
            exports: data.join("exports"),
            skills: data.join("skills"),
            prs: cache.join("prs"),
            data,
            cache,
        };
        paths.ensure()?;
        Ok(paths)
    }

    pub(crate) fn ensure(&self) -> Result<()> {
        for path in [
            &self.data,
            &self.cache,
            &self.roots,
            &self.prs,
            &self.exports,
            &self.skills,
        ] {
            std::fs::create_dir_all(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
        }
        if let Some(parent) = self.database.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        Ok(())
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

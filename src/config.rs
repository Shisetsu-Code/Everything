use crate::model::{SearchOptions, Sort};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub options: SearchOptions,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppConfig {
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<PathBuf>,
    pub bookmarks: Vec<Bookmark>,
    pub history: Vec<String>,
    pub theme: Theme,
    pub auto_scan: bool,
    pub watch: bool,
    pub page_size: usize,
    pub search_options: SearchOptions,
    pub sort: Sort,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            roots: crate::indexer::default_roots(),
            excludes: Vec::new(),
            bookmarks: Vec::new(),
            history: Vec::new(),
            theme: Theme::System,
            auto_scan: true,
            watch: true,
            page_size: 500,
            search_options: SearchOptions::default(),
            sort: Sort::default(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("No se pudo leer {}", path.display()));
            }
        };
        let mut config: Self = serde_json::from_str(&text)
            .with_context(|| format!("Configuración inválida: {}", path.display()))?;
        config.page_size = config.page_size.clamp(100, 2000);
        config.history.truncate(100);
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(path)
            .map_err(|e| e.error)
            .with_context(|| format!("No se pudo guardar {}", path.display()))?;
        Ok(())
    }
}

pub fn data_directory() -> Result<PathBuf> {
    directories::ProjectDirs::from("", "Shisetsu", "EverythingRust")
        .map(|p| p.data_local_dir().to_path_buf())
        .context("No se encontró una carpeta para los datos de la aplicación")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_roundtrip_and_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let mut config = AppConfig {
            roots: vec![temp.path().into()],
            ..AppConfig::default()
        };
        config.save(&path).unwrap();
        config.bookmarks.push(Bookmark {
            name: "Fotos".into(),
            query: "ext:jpg;png".into(),
            options: SearchOptions::default(),
        });
        config.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path).unwrap(), config);
    }
    #[test]
    fn malformed_config_is_not_silently_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(&path, b"{broken").unwrap();
        assert!(AppConfig::load(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{broken");
    }
}

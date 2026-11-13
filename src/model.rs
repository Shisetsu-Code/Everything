use serde::{Deserialize, Serialize};

/// Display metadata; unknown metadata is never represented as zero or a fake timestamp.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub parent: String,
    pub extension: String,
    pub size: Option<u64>,
    pub modified: Option<i64>,
    pub created: Option<i64>,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Empty for live local files; nonempty for imported, offline catalogs.
    pub source: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub match_path: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SortColumn {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Created,
    Extension,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Sort {
    pub column: SortColumn,
    pub descending: bool,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub id: u64,
    pub text: String,
    pub options: SearchOptions,
    pub sort: Sort,
    pub offset: u64,
    pub limit: usize,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            id: 0,
            text: String::new(),
            options: SearchOptions::default(),
            sort: Sort::default(),
            offset: 0,
            limit: 500,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub rows: Vec<FileEntry>,
    pub total: u64,
    pub offset: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Efu,
}

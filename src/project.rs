use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::context::RustAnalyzerConfig;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum TransportType {
    Stdio,
    Sse {
        host: String,
        port: u16,
    },
    StreamableHttp {
        host: String,
        port: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub root: PathBuf,
    pub ignore_crates: Vec<String>,
    #[serde(rename = "rust-analyzer")]
    pub rust_analyzer: Option<RustAnalyzerConfig>,
}

impl Project {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().canonicalize()?;
        Ok(Self {
            root,
            ignore_crates: vec![],
            rust_analyzer: None,
        })
    }

    #[inline]
    pub fn ignore_crates(&self) -> &[String] {
        &self.ignore_crates
    }

    #[inline]
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    #[inline]
    pub fn rust_analyzer(&self) -> Option<&RustAnalyzerConfig> {
        self.rust_analyzer.as_ref()
    }

    #[inline]
    pub fn uri(&self) -> Result<Url> {
        Url::from_file_path(&self.root).map_err(|()| anyhow::anyhow!("Failed to create project root URI"))
    }

    #[inline]
    pub fn file_uri(&self, relative_path: impl AsRef<Path>) -> Result<Url> {
        Url::from_file_path(self.root.join(relative_path)).map_err(|()| anyhow::anyhow!("Failed to create file URI"))
    }
}

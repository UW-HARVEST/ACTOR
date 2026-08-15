use anyhow::Result;
use std::path::{Path, PathBuf};

/// Relative, non-empty, no `..`: cannot escape the tree it indexes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct RelPath(PathBuf);

impl RelPath {
    pub fn new(p: impl AsRef<Path>) -> Result<Self> {
        let p = p.as_ref();
        anyhow::ensure!(p.is_relative(), "path must be relative: {}", p.display());
        anyhow::ensure!(
            !p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "path must not contain `..`: {}",
            p.display()
        );
        anyhow::ensure!(p.as_os_str() != "", "path must not be empty");
        Ok(Self(p.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_rejects_escapes() {
        assert!(RelPath::new("a/b").is_ok());
        assert!(RelPath::new("/abs").is_err(), "absolute must be refused");
        assert!(
            RelPath::new("../up").is_err(),
            "parent traversal must be refused"
        );
        assert!(RelPath::new("").is_err());
    }
}

use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{Array, DocumentMut, Item, Table};

pub struct CargoToml {
    doc: DocumentMut,
    path: std::path::PathBuf,
}

impl CargoToml {
    pub fn open(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let doc: DocumentMut = content
            .parse()
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Self {
            doc,
            path: path.to_path_buf(),
        })
    }

    pub fn save(&self) -> Result<()> {
        std::fs::write(&self.path, self.doc.to_string())
            .with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn set_lib(&mut self, name: &str) {
        let lib = self
            .doc
            .entry("lib")
            .or_insert_with(|| Item::Table(Table::new()));
        if let Some(t) = lib.as_table_mut() {
            t.insert("name", toml_edit::value(name));
            let mut arr = Array::new();
            arr.push("cdylib");
            t.insert("crate-type", toml_edit::value(arr));
        }
    }

    pub fn set_bin_driver(&mut self) {
        if let Some(bins) = self
            .doc
            .get_mut("bin")
            .and_then(|b| b.as_array_of_tables_mut())
        {
            if let Some(bin) = bins.iter_mut().next() {
                bin.insert("name", toml_edit::value("driver"));
                return;
            }
        }
        let mut bin = Table::new();
        bin.insert("name", toml_edit::value("driver"));
        bin.insert("path", toml_edit::value("src/main.rs"));
        let mut arr = toml_edit::ArrayOfTables::new();
        arr.push(bin);
        self.doc.insert("bin", Item::ArrayOfTables(arr));
    }

    pub fn remove_bin(&mut self) {
        self.doc.remove("bin");
    }

    pub fn add_workspace(&mut self) {
        if self.doc.get("workspace").is_none() {
            self.doc.insert("workspace", Item::Table(Table::new()));
        }
    }

    pub fn set_default_features(&mut self, features: &[String]) {
        let feat = self
            .doc
            .entry("features")
            .or_insert_with(|| Item::Table(Table::new()));
        if let Some(t) = feat.as_table_mut() {
            let mut arr = Array::new();
            for f in features {
                arr.push(f.as_str());
            }
            t.insert("default", toml_edit::value(arr));
        }
    }

    pub fn defined_features(&self) -> Vec<String> {
        self.doc
            .get("features")
            .and_then(|f| f.as_table())
            .map(|t| {
                t.iter()
                    .map(|(k, _)| k.to_string())
                    .filter(|k| k != "default")
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub fn strip_for_lib(translated_rust_dir: &Path) -> Result<()> {
    let main_rs = translated_rust_dir.join("src/main.rs");
    if main_rs.exists() {
        std::fs::remove_file(&main_rs)?;
    }
    let tests_dir = translated_rust_dir.join("tests");
    if tests_dir.exists() {
        std::fs::remove_dir_all(&tests_dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_cargo_toml(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("Cargo.toml");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn set_lib_name_and_cdylib() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let path = write_cargo_toml(
            tmp.path(),
            "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\n",
        );
        let mut cargo = CargoToml::open(&path).unwrap();
        cargo.set_lib("blake");
        cargo.save().unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(r#"name = "blake""#));
        assert!(content.contains(r#""cdylib""#));
    }

    #[test]
    fn set_bin_driver() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let path = write_cargo_toml(tmp.path(), "[package]\nname = \"mybin\"\n");
        let mut cargo = CargoToml::open(&path).unwrap();
        cargo.set_bin_driver();
        cargo.save().unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(r#"name = "driver""#));
    }

    #[test]
    fn remove_bin_section() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let path = write_cargo_toml(
            tmp.path(),
            "[package]\nname = \"mylib\"\n\n[[bin]]\nname = \"driver\"\npath = \"src/main.rs\"\n",
        );
        let mut cargo = CargoToml::open(&path).unwrap();
        cargo.remove_bin();
        cargo.save().unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("[[bin]]"));
        assert!(!content.contains("driver"));
    }

    #[test]
    fn add_workspace_idempotent() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let path = write_cargo_toml(tmp.path(), "[package]\nname = \"test\"\n\n[workspace]\n");
        let mut cargo = CargoToml::open(&path).unwrap();
        cargo.add_workspace();
        cargo.save().unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("[workspace]").count(), 1);
    }

    #[test]
    fn set_default_features() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let path = write_cargo_toml(
            tmp.path(),
            "[package]\nname = \"test\"\n\n[features]\nblake = []\nsha2 = []\n\"128f\" = []\n",
        );
        let mut cargo = CargoToml::open(&path).unwrap();
        cargo.set_default_features(&["blake".into(), "128f".into()]);
        cargo.save().unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("default"));
        assert!(content.contains("blake"));
        assert!(content.contains("128f"));
    }

    #[test]
    fn strip_for_lib_removes_main_and_tests() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let dir = tmp.path().join(crate::battery::TRANSLATED_RUST);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.join("src/lib.rs"), "pub fn foo() {}").unwrap();
        fs::write(dir.join("tests/test.rs"), "#[test] fn t() {}").unwrap();

        strip_for_lib(&dir).unwrap();

        assert!(!dir.join("src/main.rs").exists());
        assert!(!dir.join("tests").exists());
        assert!(dir.join("src/lib.rs").exists());
    }

    #[test]
    fn strip_for_lib_noop_when_missing() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let dir = tmp.path().join(crate::battery::TRANSLATED_RUST);
        fs::create_dir_all(dir.join("src")).unwrap();
        strip_for_lib(&dir).unwrap();
    }
}

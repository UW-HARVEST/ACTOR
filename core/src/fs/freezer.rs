use super::{DiagnosticsDir, Dir, DirEntry, File, Symlink, dir::GetNofollowError};
use std::collections::HashMap;
use std::fs::{read_dir, symlink_metadata};
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// `Freezer` implements freezing filesystem objects (making them readonly and constructing a
/// `DirEntry` pointing to them -- see the `fs` module documentation for more information). To
/// avoid repeatedly freezing the same directories, it keeps track of which filesystem objects have
/// already been frozen.
pub(crate) struct Freezer {
    diagnostics_dir: Arc<DiagnosticsDir>,
    // Paths are relative to the diagnostic directory, and do not contain symlinks, `.`, or `..`.
    // Nested frozen paths are removed.
    frozen: HashMap<PathBuf, DirEntry>,
}

impl Freezer {
    pub fn new(diagnostics_dir: Arc<DiagnosticsDir>) -> Freezer {
        Freezer {
            diagnostics_dir,
            frozen: HashMap::new(),
        }
    }

    /// Makes a read-only copy of a filesystem object in the diagnostic directory. `path` must be
    /// relative to the diagnostics directory, and cannot contain `..` or symlinks.
    pub fn copy_ro<P: AsRef<Path>>(&mut self, path: P, entry: DirEntry) -> io::Result<()> {
        use DirectVerified::Unfrozen;
        let Unfrozen {
            mut absolute,
            relative,
        } = self.verify_direct(path.as_ref())?
        else {
            return Err(ErrorKind::PermissionDenied.into());
        };
        entry.copy_ro(&mut absolute, &mut relative.clone())?;
        let prev = self.frozen.insert(relative, entry);
        debug_assert!(prev.is_none());
        Ok(())
    }

    /// Makes a read-write copy of a filesystem object in the diagnostic directory. `path` must be
    /// relative to the diagnostics directory, and cannot contain `..` or symlinks.
    pub fn copy_rw<P: AsRef<Path>>(&mut self, path: P, entry: DirEntry) -> io::Result<()> {
        let _ = (path, entry);
        todo!()
    }

    /// Freezes the given path, returning an object referencing it. `path` must be relative to the
    /// diagnostics directory, and cannot contain `..`. This will not follow symlinks (i.e. `path`
    /// cannot have symlinks in its directory path, and if `path` points to a symlink then a
    /// Symlink will be returned).
    pub fn freeze<P: AsRef<Path>>(&mut self, path: P) -> io::Result<DirEntry> {
        self.freeze_inner(path.as_ref())
    }

    /// The implementation of [freeze]. The only difference is that this is not generic.
    fn freeze_inner(&mut self, path: &Path) -> io::Result<DirEntry> {
        // We first call verify_direct, which does two things for us:
        // 1. Check if the path is already frozen
        // 2. Verify the path is direct, if it is not frozen.
        let (mut absolute, mut relative) = match self.verify_direct(path)? {
            // verify_direct returns early if `path` points into an already-frozen directory. In
            // that case, call `Dir::get_nofollow` to finish the path traversal.
            DirectVerified::Frozen {
                dir_entry: DirEntry::Dir(dir),
                remaining,
            } => {
                return match dir.get_nofollow(remaining) {
                    Ok(entry) => Ok(entry),
                    Err(GetNofollowError::LeavesDir) => Err(ErrorKind::InvalidInput.into()),
                    Err(GetNofollowError::NotADirectory) => Err(ErrorKind::NotADirectory.into()),
                    Err(GetNofollowError::NotFound) => Err(ErrorKind::NotFound.into()),
                };
            }
            // For frozen non-directory paths, dir_entry is exactly the entry at `path`.
            DirectVerified::Frozen {
                dir_entry,
                remaining: _,
            } => return Ok(dir_entry.clone()),
            // If the path is not already frozen, continue below.
            DirectVerified::Unfrozen { absolute, relative } => (absolute, relative),
        };
        // The path is not frozen yet. Determine whether it is a file, directory, or symlink, and
        // call the appropriate function to freeze it.
        let metadata = symlink_metadata(&absolute)?;
        let entry = match metadata.file_type() {
            file_type if file_type.is_file() => DirEntry::File(File::new(
                self.diagnostics_dir.clone(),
                absolute,
                relative.clone(),
                metadata.permissions(),
            )?),
            file_type if file_type.is_dir() => self.build_dir(&mut absolute, &mut relative)?.into(),
            _ => DirEntry::Symlink(Symlink::new(&absolute)?),
        };
        self.frozen.insert(relative, entry.clone());
        Ok(entry)
    }

    /// Recursive function to freeze and build a Dir. Used by freeze_inner().
    fn build_dir(&mut self, absolute: &mut PathBuf, relative: &mut PathBuf) -> io::Result<Dir> {
        let mut contents = HashMap::new();
        for entry in read_dir(&absolute)? {
            let entry = entry?;
            let file_name = entry.file_name();
            relative.push(&file_name);
            let new_entry = if let Some(entry) = self.frozen.remove(&*relative) {
                entry
            } else {
                absolute.push(&file_name);
                let file_type = entry.file_type()?;
                let entry = match () {
                    _ if file_type.is_file() => File::new(
                        self.diagnostics_dir.clone(),
                        absolute.clone(),
                        relative.clone(),
                        entry.metadata()?.permissions(),
                    )?
                    .into(),
                    _ if file_type.is_dir() => self.build_dir(absolute, relative)?.into(),
                    _ => Symlink::new(&absolute)?.into(),
                };
                absolute.pop();
                entry
            };
            contents.insert(file_name, new_entry);
            relative.pop();
        }
        Dir::new(absolute, contents)
    }

    /// Verifies the passed path is a direct path within the diagnostic directory (must be relative
    /// and not contain `..` or symlinks). The passed path does not need to exist, but its parent
    /// directory does.
    ///
    /// If the passed path is under a frozen directory, verification terminates early and the
    /// directory and remaining path are returned. In this case, the portion of `path` pointing to
    /// the frozen directory is verified to be a direct path, but the remaining `path` is not
    /// verified. If you want to verify the remainder of the path, call `Dir::get_nofollow` on it.
    /// This is done to avoid unnecessary lookups if the calling function is just going to error on
    /// an already-frozen path (as e.g. copy_ro does).
    fn verify_direct<'s, 'p>(&'s self, path: &'p Path) -> io::Result<DirectVerified<'s, 'p>> {
        // It is conceivable that the diagnostic itself could be frozen, perhaps at the end of
        // translate's execution. Since the first loop (below) won't check that case, we check for
        // it up front.
        if let Some(dir_entry) = self.frozen.get(AsRef::<Path>::as_ref("")) {
            return Ok(DirectVerified::Frozen {
                dir_entry,
                remaining: path,
            });
        }
        // This first loop performs all the checks we can perform in-memory.
        // `relative` is the current path relative to the diagnostic directory.
        let mut relative = PathBuf::with_capacity(path.as_os_str().len());
        let mut components = path.components();
        for component in &mut components {
            // Any component type other than CurDir and Normal means that `path` is not direct.
            // Ignore CurDir (because of Components' normalization, it can only refer to the
            // diagnostic directory).
            let name = match component {
                Component::Normal(name) => name,
                Component::CurDir => continue,
                _ => return Err(ErrorKind::InvalidInput.into()),
            };
            relative.push(name);
            let Some(dir_entry) = self.frozen.get(&relative) else {
                continue;
            };
            // This path is already frozen. This can be an error or a success, depending on whether
            // there are more path components remaining and on whether this is a directory.
            let remaining = components.as_path();
            if !dir_entry.is_dir() && !remaining.as_os_str().is_empty() {
                // `path` tries to traverse through a file or symlink, which should error.
                return Err(ErrorKind::NotADirectory.into());
            }
            // As described in the function documentation, we terminate early in this case (no need
            // to check filesystem metadata, as the path being frozen means that its parents are
            // not symlinks).
            return Ok(DirectVerified::Frozen {
                dir_entry,
                remaining,
            });
        }
        // This second loop verifies that path does not traverse through a directory or symlink.
        // `absolute` is the current path as an absolute path (for filesystem operations).
        let mut absolute = self.diagnostics_dir.path().to_path_buf();
        let mut iter = path.iter();
        // Drop the last component to make the loop skip `path` itself (we don't need to check if
        // `path` exists or what type of filesystem object it is).
        let last = iter.next_back();
        for name in iter {
            absolute.push(name);
            // Verify that the filesystem object that `absolute` points to is a directory.
            if !symlink_metadata(&absolute)?.file_type().is_dir() {
                return Err(ErrorKind::NotADirectory.into());
            }
        }
        // Append the previously-dropped last component to make `absolute` correct (if path is
        // empty, then append nothing).
        if let Some(last) = last {
            absolute.push(last)
        };
        Ok(DirectVerified::Unfrozen { absolute, relative })
    }
}

/// Return value from verify_direct
#[derive(Debug)]
enum DirectVerified<'s, 'p> {
    /// The path being verified is already frozen.
    Frozen {
        /// The DirEntry of the frozen path. This may be an ancesor of the passed path.
        dir_entry: &'s DirEntry,
        /// The remainder of `path`. This has not been verified to be direct.
        remaining: &'p Path,
    },
    /// The path is not frozen yet.
    Unfrozen {
        /// The input path, but as an absolute path for filesystem operations.
        absolute: PathBuf,
        /// The input path without `.` entries.
        relative: PathBuf,
    },
}

#[cfg(all(not(miri), test))]
mod tests {
    use super::super::test_util::dir_has_entries;
    use super::*;
    use std::fs::{create_dir, create_dir_all, read_link, set_permissions, write};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::ptr;

    impl<'s, 'p> DirectVerified<'s, 'p> {
        fn get_frozen(&self) -> Option<(&'s DirEntry, &'p Path)> {
            match self {
                DirectVerified::Frozen {
                    dir_entry,
                    remaining,
                } => Some((dir_entry, remaining)),
                DirectVerified::Unfrozen { .. } => None,
            }
        }
    }

    /// Returns true iff the object at this path is read-only.
    fn is_readonly<P: AsRef<Path>>(path: P) -> bool {
        symlink_metadata(path).unwrap().permissions().readonly()
    }

    #[test]
    fn copy_ro() {
        let diagnostics_dir = Arc::new(DiagnosticsDir::tempdir().unwrap());
        let mut freezer = Freezer::new(diagnostics_dir.clone());
        // Build the following filesystem structure:
        //
        // a/b/absolute  An absolute symlink to a/target
        // a/b/c/file1   A file
        // a/b/file2     A file
        // a/b/relative  A relative symlink to a/target
        // a/target      A file
        let a_b_absolute = diagnostics_dir.to_absolute_path("a/b/absolute").unwrap();
        let a_b_c_file1 = diagnostics_dir.to_absolute_path("a/b/c/file1").unwrap();
        let a_b_file2 = diagnostics_dir.to_absolute_path("a/b/file2").unwrap();
        let a_b_relative = diagnostics_dir.to_absolute_path("a/b/relative").unwrap();
        let a_target = diagnostics_dir.to_absolute_path("a/target").unwrap();
        create_dir_all(diagnostics_dir.to_absolute_path("a/b/c").unwrap()).unwrap();
        symlink(&a_target, a_b_absolute).unwrap();
        write(a_b_c_file1, "file1\n").unwrap();
        write(a_b_file2, "file2\n").unwrap();
        symlink("../target", a_b_relative).unwrap();
        write(&a_target, "target\n").unwrap();
        // Freeze a/b, then move it to a/new.
        let a_b = freezer.freeze("a/b").unwrap();
        freezer.copy_ro("a/new", a_b).unwrap();
        let a_new = diagnostics_dir.to_absolute_path("a/new").unwrap();
        let a_new_c = diagnostics_dir.to_absolute_path("a/new/c").unwrap();
        let a_new_absolute = diagnostics_dir.to_absolute_path("a/new/absolute").unwrap();
        let a_new_c_file1 = diagnostics_dir.to_absolute_path("a/new/c/file1").unwrap();
        let a_new_file2 = diagnostics_dir.to_absolute_path("a/new/file2").unwrap();
        let a_new_relative = diagnostics_dir.to_absolute_path("a/new/relative").unwrap();
        // Verify the directories were created rather than being symlinked.
        assert!(symlink_metadata(&a_new).unwrap().is_dir());
        assert!(symlink_metadata(&a_new_c).unwrap().is_dir());
        // Verify the directories under a/new are read-only.
        assert!(is_readonly(a_new));
        assert!(is_readonly(a_new_c));
        // Verify all the symlinks were moved correctly.
        assert_eq!(read_link(a_new_absolute).unwrap(), a_target);
        assert_eq!(
            read_link(a_new_c_file1).unwrap().as_path(),
            "../../b/c/file1"
        );
        assert_eq!(read_link(a_new_file2).unwrap().as_path(), "../b/file2");
        assert_eq!(read_link(a_new_relative).unwrap().as_path(), "../target");
    }

    #[test]
    fn freeze() {
        use ErrorKind::{InvalidInput, NotADirectory, NotFound};
        let diagnostics_dir = Arc::new(DiagnosticsDir::tempdir().unwrap());
        let mut freezer = Freezer::new(diagnostics_dir.clone());
        // Build the following filesystem structure under the diagnostics directory:
        //
        // a/outer_file       A basic file
        // a/b/absolute_link  An absolute symlink (should not be followed).
        // a/b/c              A subdirectory.
        // a/b/inner_file     A basic file
        // a/b/dir_link       A symlink to a/
        // a/b/file_link      A symlink to a/outer_file
        let a = PathBuf::from_iter([diagnostics_dir.path(), "a".as_ref()]);
        let a_b = PathBuf::from_iter([a.as_path(), "b".as_ref()]);
        let a_b_absolute_link = PathBuf::from_iter([a_b.as_path(), "absolute_link".as_ref()]);
        let a_b_c = PathBuf::from_iter([a_b.as_path(), "c".as_ref()]);
        let a_b_inner_file = PathBuf::from_iter([a_b.as_path(), "inner_file".as_ref()]);
        let a_b_dir_link = PathBuf::from_iter([a_b.as_path(), "dir_link".as_ref()]);
        let a_b_file_link = PathBuf::from_iter([a_b.as_path(), "file_link".as_ref()]);
        let a_outer_file = PathBuf::from_iter([a.as_path(), "outer_file".as_ref()]);
        create_dir(&a).unwrap();
        write(&a_outer_file, "outer_file").unwrap();
        create_dir(&a_b).unwrap();
        symlink("/absolute", a_b_absolute_link).unwrap();
        create_dir(&a_b_c).unwrap();
        write(&a_b_inner_file, "").unwrap();
        symlink("../..", a_b_dir_link).unwrap();
        symlink("../outer_file", a_b_file_link).unwrap();
        // Try freezing a few invalid paths first.
        let error = freezer.freeze("/absolute").unwrap_err();
        assert_eq!(error.kind(), InvalidInput);
        let error = freezer.freeze("a/outer_file/c").unwrap_err();
        assert_eq!(error.kind(), NotADirectory);
        let error = freezer.freeze("a/b/dir_link/b").unwrap_err();
        assert_eq!(error.kind(), NotADirectory);
        assert_eq!(freezer.freeze("nonexistent").unwrap_err().kind(), NotFound);
        // Freeze a/b/inner_file
        let entry = freezer.freeze("a/b/inner_file").unwrap();
        assert!(entry.file().is_some());
        assert!(!is_readonly(&a_b));
        let mut inner_file_perms = symlink_metadata(&a_b_inner_file).unwrap().permissions();
        assert!(inner_file_perms.readonly());
        // Freeze a/b/dir_link
        let a_b_dir_link_symlink = freezer.freeze("a/b/dir_link").unwrap().symlink().unwrap();
        assert_eq!(a_b_dir_link_symlink.contents(), "../..");
        assert!(!is_readonly(&a_b));
        // Freezer should not re-freeze a/b/inner_file and a/b/dir_link when a/b is frozen.
        // However, there's not a great way to verify that. What we do here is:
        // 1. For a/b/inner_file, we make the file writable again, then verify that it is still
        //    writable after freezing a/b.
        // 2. For a/b/dir_link, we verify the returned path has the same address as the first copy.
        inner_file_perms.set_mode(0o644);
        set_permissions(&a_b_inner_file, inner_file_perms).unwrap();
        // Freeze a/b
        let a_b_dir = freezer.freeze("a/b").unwrap().dir().unwrap();
        let entry = |n| a_b_dir.get_nofollow(n).unwrap();
        let absolute_link_symlink = entry("absolute_link").symlink().unwrap();
        assert_eq!(absolute_link_symlink.contents(), "/absolute");
        assert_eq!(entry("c").dir().unwrap().entries().count(), 0);
        assert!(entry("inner_file").file().is_some());
        let a_b_dir_link_symlink_2 = entry("dir_link").symlink().unwrap();
        assert_eq!(a_b_dir_link_symlink_2.contents(), "../..");
        let file_link_symlink = entry("file_link").symlink().unwrap();
        assert_eq!(file_link_symlink.contents(), "../outer_file");
        assert_eq!(a_b_dir.entries().count(), 5);
        // Verify that everything has the expected permissions.
        assert!(!is_readonly(&a));
        let mut a_b_perms = symlink_metadata(&a_b).unwrap().permissions();
        assert!(a_b_perms.readonly());
        let mut a_b_c_perms = symlink_metadata(&a_b_c).unwrap().permissions();
        assert!(a_b_c_perms.readonly());
        assert!(!is_readonly(&a_b_inner_file)); // Verifies a/b/inner_file not re-frozen
        assert!(!is_readonly(&a_outer_file));
        // Check that a/b/dir_link has the same path.
        assert!(ptr::eq(
            a_b_dir_link_symlink.contents(),
            a_b_dir_link_symlink_2.contents()
        ));
        // Repeat the previous readonly trick to verify that freezing a/ does not re-freeze
        // anything under b/.
        a_b_perms.set_mode(0o755);
        a_b_c_perms.set_mode(0o755);
        set_permissions(&a_b, a_b_perms).unwrap();
        set_permissions(&a_b_c, a_b_c_perms).unwrap();
        // Freeze a/
        let a_dir = freezer.freeze("a").unwrap().dir().unwrap();
        let a_b_dir_2 = a_dir.get_nofollow("b").unwrap().dir().unwrap();
        assert!(a_dir.get_nofollow("outer_file").unwrap().file().is_some());
        assert_eq!(a_dir.entries().count(), 2);
        assert!(dir_has_entries(
            &a_b_dir_2,
            &["absolute_link", "c", "dir_link", "file_link", "inner_file"]
        ));
        assert!(is_readonly(&a));
        assert!(!is_readonly(&a_b)); // Should not re-freeze
        assert!(!is_readonly(&a_b_c)); // Should not re-freeze
        assert!(!is_readonly(&a_b_inner_file)); // Should not re-freeze
        assert!(is_readonly(&a_outer_file));
        // Verify that the nested frozen paths were removed.
        assert_eq!(freezer.frozen.len(), 1);
        assert_eq!(freezer.frozen.iter().next().map(|(n, _)| n).unwrap(), "a");
    }

    #[test]
    fn verify_direct() {
        let diagnostics_dir = Arc::new(DiagnosticsDir::tempdir().unwrap());
        let mut freezer = Freezer::new(diagnostics_dir.clone());
        // Build the following filesystem structure under the diagnostic directory:
        //
        // subdir/file  A text file.
        // link         A symlink to `subdir`
        let subdir = diagnostics_dir.to_absolute_path("subdir").unwrap();
        let file = diagnostics_dir.to_absolute_path("subdir/file").unwrap();
        let link = diagnostics_dir.to_absolute_path("link").unwrap();
        create_dir(subdir).unwrap();
        write(&file, "file contents").unwrap();
        symlink("subdir", link).unwrap();
        // Test with paths that should fail with inspection alone.
        let result = freezer.verify_direct("/absolute_path".as_ref());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
        let result = freezer.verify_direct("subdir/../symlink".as_ref());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
        // Test paths that try to traverse through nonexistent directories, files, and symlinks.
        let result = freezer.verify_direct("subdir/file/target".as_ref());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotADirectory);
        let result = freezer.verify_direct("nonexistent/file".as_ref());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound);
        let result = freezer.verify_direct("link/file".as_ref());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotADirectory);
        // Test a path pointing to a target that exists.
        let result = freezer.verify_direct("subdir/file".as_ref());
        let DirectVerified::Unfrozen { absolute, relative } = result.unwrap() else {
            panic!("Unexpected Frozen return for subdir/file");
        };
        assert_eq!(absolute, file);
        assert_eq!(relative.as_path(), "subdir/file");
        // Test a path with some unnecessary `.` entries (verify the path returned removes them).
        let result = freezer.verify_direct("./subdir/./file".as_ref());
        let DirectVerified::Unfrozen { absolute, relative } = result.unwrap() else {
            panic!("Unexpected Frozen return for ./subdir/./file");
        };
        assert_eq!(absolute, file);
        assert_eq!(relative.as_path(), "subdir/file");
        // Test a path pointing to a target that does not exist.
        let result = freezer.verify_direct("subdir/nonexistent".as_ref());
        let DirectVerified::Unfrozen { absolute, relative } = result.unwrap() else {
            panic!("Unexpected Frozen return for subdir/nonexistent");
        };
        assert_eq!(
            absolute,
            diagnostics_dir
                .to_absolute_path("subdir/nonexistent")
                .unwrap()
        );
        assert_eq!(relative.as_path(), "subdir/nonexistent");
        // Freeze subdir/file and link, verifying that looking up each directly return the correct
        // DirEntry type.
        freezer.freeze("subdir/file").unwrap();
        let result = freezer.verify_direct("subdir/file".as_ref());
        let (dir_entry, remaining) = result.unwrap().get_frozen().unwrap();
        assert!(dir_entry.is_file());
        assert_eq!(remaining, "");
        freezer.freeze("link").unwrap();
        let result = freezer.verify_direct("link".as_ref());
        let (dir_entry, remaining) = result.unwrap().get_frozen().unwrap();
        assert!(dir_entry.is_symlink());
        assert_eq!(remaining, "");
        // Freeze subdir, verify both that looking up subdir directly and looking up subdir/file
        // find the frozen subdir.
        freezer.freeze("subdir").unwrap();
        let result = freezer.verify_direct("subdir".as_ref());
        let (dir_entry, remaining) = result.unwrap().get_frozen().unwrap();
        assert!(dir_entry.dir().unwrap().get_entry("file").is_some());
        assert_eq!(remaining, "");
        let result = freezer.verify_direct("subdir/file".as_ref());
        let (dir_entry, remaining) = result.unwrap().get_frozen().unwrap();
        assert!(dir_entry.dir().unwrap().get_entry("file").is_some());
        assert_eq!(remaining, "file");
        // A nonexistent file under subdir should still show as Frozen, as verify_direct()
        // shouldn't traverse into the filesystem at all.
        let result = freezer.verify_direct("subdir/nonexistent".as_ref());
        let (dir_entry, remaining) = result.unwrap().get_frozen().unwrap();
        assert!(dir_entry.dir().unwrap().get_entry("file").is_some());
        assert_eq!(remaining, "nonexistent");
        // Last: freeze the entire diagnostics directory, verify that it always returns Frozen.
        freezer.freeze("").unwrap();
        let result = freezer.verify_direct("link".as_ref());
        let (dir_entry, remaining) = result.unwrap().get_frozen().unwrap();
        assert!(dir_entry.dir().unwrap().get_entry("link").is_some());
        assert_eq!(remaining, "link");
        // Because "" is frozen, a nonexistent file should still return Frozen, because it
        // shouldn't traverse the filesystem at all.
        let result = freezer.verify_direct("nonexistent".as_ref());
        let (dir_entry, remaining) = result.unwrap().get_frozen().unwrap();
        assert!(dir_entry.dir().unwrap().get_entry("link").is_some());
        assert_eq!(remaining, "nonexistent");
    }
}

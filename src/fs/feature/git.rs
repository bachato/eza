// SPDX-FileCopyrightText: 2024 Christina Sørensen
// SPDX-License-Identifier: EUPL-1.2
//
// SPDX-FileCopyrightText: 2023-2024 Christina Sørensen, eza contributors
// SPDX-FileCopyrightText: 2014 Benjamin Sago
// SPDX-License-Identifier: MIT
//! Getting the Git status of files and directories.

use std::env;
use std::ffi::OsStr;
#[cfg(target_family = "unix")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use git2::StatusEntry;
use log::{debug, error, info, warn};

use crate::fs::fields as f;

/// A **Git cache** is assembled based on the user’s input arguments.
///
/// This uses vectors to avoid the overhead of hashing: it’s not worth it when the
/// expected number of Git repositories per exa invocation is 0 or 1...
pub struct GitCache {
    /// A list of discovered Git repositories and their paths.
    repos: Vec<GitRepo>,

    /// Paths that we’ve confirmed do not have Git repositories underneath them.
    misses: Vec<PathBuf>,
}

impl GitCache {
    #[must_use]
    pub fn has_anything_for(&self, index: &Path) -> bool {
        self.repos.iter().any(|e| e.has_path(index))
    }

    #[must_use]
    pub fn get(&self, index: &Path, prefix_lookup: bool) -> f::Git {
        self.repos
            .iter()
            .find(|repo| repo.has_path(index))
            .map(|repo| repo.search(index, prefix_lookup))
            .unwrap_or_default()
    }
}

use std::iter::FromIterator;
impl FromIterator<PathBuf> for GitCache {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let iter = iter.into_iter();
        let mut git = Self {
            repos: Vec::with_capacity(iter.size_hint().0),
            misses: Vec::new(),
        };

        if let Ok(path) = env::var("GIT_DIR") {
            // These flags are consistent with how `git` uses GIT_DIR:
            let flags = git2::RepositoryOpenFlags::NO_SEARCH | git2::RepositoryOpenFlags::NO_DOTGIT;
            match GitRepo::discover(path.into(), flags) {
                Ok(repo) => {
                    debug!("Opened GIT_DIR repo");
                    git.repos.push(repo);
                }
                Err(miss) => {
                    git.misses.push(miss);
                }
            }
        }

        for path in iter {
            if git.misses.contains(&path) {
                debug!("Skipping {path:?} because it already came back Gitless");
            } else if git.repos.iter().any(|e| e.has_path(&path)) {
                debug!("Skipping {path:?} because we already queried it");
            } else {
                let flags = git2::RepositoryOpenFlags::FROM_ENV;
                match GitRepo::discover(path, flags) {
                    Ok(r) => {
                        if let Some(r2) = git.repos.iter_mut().find(|e| e.has_workdir(&r.workdir)) {
                            debug!(
                                "Adding to existing repo (workdir matches with {:?})",
                                r2.workdir
                            );
                            r2.extra_paths.push(r.original_path);
                            continue;
                        }

                        debug!("Discovered new Git repo");
                        git.repos.push(r);
                    }
                    Err(miss) => {
                        git.misses.push(miss);
                    }
                }
            }
        }

        git
    }
}

/// A **Git repository** is one we’ve discovered somewhere on the filesystem.
pub struct GitRepo {
    /// The queryable contents of the repository: either a `git2` repo, or the
    /// cached results from when we queried it last time.
    contents: Mutex<GitContents>,

    /// The working directory of this repository.
    /// This is used to check whether two repositories are the same.
    workdir: PathBuf,

    /// The path that was originally checked to discover this repository.
    /// This is as important as the `extra_paths` (it gets checked first), but
    /// is separate to avoid having to deal with a non-empty Vec.
    original_path: PathBuf,

    /// Any other paths that were checked only to result in this same
    /// repository.
    extra_paths: Vec<PathBuf>,
}

/// A repository’s queried state.
enum GitContents {
    /// All the interesting Git stuff goes through this.
    Before { repo: git2::Repository },

    /// Temporary value used in `repo_to_statuses` so we can move the
    /// repository out of the `Before` variant.
    Processing,

    /// The data we’ve extracted from the repository, but only after we’ve
    /// actually done so.
    After { statuses: Git },
}

impl GitRepo {
    /// Searches through this repository for a path (to a file or directory,
    /// depending on the prefix-lookup flag) and returns its Git status.
    ///
    /// Actually querying the `git2` repository for the mapping of paths to
    /// Git statuses is only done once, and gets cached so we don’t need to
    /// re-query the entire repository the times after that.
    ///
    /// The temporary `Processing` enum variant is used after the `git2`
    /// repository is moved out, but before the results have been moved in!
    /// See <https://stackoverflow.com/q/45985827/3484614>
    fn search(&self, index: &Path, prefix_lookup: bool) -> f::Git {
        use std::mem::replace;

        let mut contents = self.contents.lock().unwrap();
        if let GitContents::After { ref statuses } = *contents {
            debug!("Git repo {:?} has been found in cache", &self.workdir);
            return statuses.status(index, prefix_lookup);
        }

        debug!("Querying Git repo {:?} for the first time", &self.workdir);
        let repo = replace(&mut *contents, GitContents::Processing).inner_repo();
        let statuses = repo_to_statuses(&repo, &self.workdir);
        let result = statuses.status(index, prefix_lookup);
        let _processing = replace(&mut *contents, GitContents::After { statuses });
        result
    }

    /// Whether this repository has the given working directory.
    fn has_workdir(&self, path: &Path) -> bool {
        self.workdir == path
    }

    /// Whether this repository cares about the given path at all.
    fn has_path(&self, path: &Path) -> bool {
        path.starts_with(&self.original_path)
            || self.extra_paths.iter().any(|e| path.starts_with(e))
    }

    /// Open a Git repository. Depending on the flags, the path is either
    /// the repository's "gitdir" (or a "gitlink" to the gitdir), or the
    /// path is the start of a rootwards search for the repository.
    fn discover(path: PathBuf, flags: git2::RepositoryOpenFlags) -> Result<Self, PathBuf> {
        info!("Opening Git repository for {path:?} ({flags:?})");
        let unused: [&OsStr; 0] = [];
        let repo = match git2::Repository::open_ext(&path, flags, unused) {
            Ok(r) => r,
            Err(e) => {
                error!("Error opening Git repository for {path:?}: {e:?}");
                return Err(path);
            }
        };

        if let Some(workdir) = repo.workdir() {
            let workdir = workdir.to_path_buf();
            let contents = Mutex::new(GitContents::Before { repo });
            Ok(Self {
                contents,
                workdir,
                original_path: path,
                extra_paths: Vec::new(),
            })
        } else {
            warn!("Repository has no workdir?");
            Err(path)
        }
    }
}

impl GitContents {
    /// Assumes that the repository hasn’t been queried, and extracts it
    /// (consuming the value) if it has. This is needed because the entire
    /// enum variant gets replaced when a repo is queried (see above).
    fn inner_repo(self) -> git2::Repository {
        if let Self::Before { repo } = self {
            repo
        } else {
            unreachable!("Tried to extract a non-Repository")
        }
    }
}

/// Iterates through a repository’s statuses, consuming it and returning the
/// mapping of files to their Git status.
/// We will have already used the working directory at this point, so it gets
/// passed in rather than deriving it from the `Repository` again.
fn repo_to_statuses(repo: &git2::Repository, workdir: &Path) -> Git {
    let mut statuses = Vec::new();

    info!("Getting Git statuses for repo with workdir {workdir:?}");
    match repo.statuses(None) {
        Ok(es) => {
            for e in es.iter() {
                if let Some(p) = get_path_from_status_entry(&e) {
                    let elem = (workdir.join(p), e.status());
                    statuses.push(elem);
                }
            }
            // We manually add the `.git` at the root of the repo as ignored, since it is in practice.
            // Also we want to avoid `eza --tree --all --git-ignore` to display files inside `.git`.
            statuses.push((workdir.join(".git"), git2::Status::IGNORED));
        }
        Err(e) => {
            error!("Error looking up Git statuses: {e:?}");
        }
    }

    Git { statuses }
}

#[allow(clippy::unnecessary_wraps)]
fn get_path_from_status_entry(e: &StatusEntry<'_>) -> Option<PathBuf> {
    #[cfg(target_family = "unix")]
    return Some(PathBuf::from(OsStr::from_bytes(e.path_bytes())));
    #[cfg(not(target_family = "unix"))]
    return if let Some(p) = e.path() {
        Some(PathBuf::from(p))
    } else {
        info!("Git status ignored for non ASCII path {:?}", e.path_bytes());
        None
    };
}

// The `repo.statuses` call above takes a long time. exa debug output:
//
//   20.311276  INFO:exa::fs::feature::git: Getting Git statuses for repo with workdir "/vagrant/"
//   20.799610  DEBUG:exa::output::table: Getting Git status for file "./Cargo.toml"
//
// Even inserting another logging line immediately afterwards doesn’t make it
// look any faster.

/// Container of Git statuses for all the files in this folder’s Git repository.
struct Git {
    statuses: Vec<(PathBuf, git2::Status)>,
}

impl Git {
    /// Get either the file or directory status for the given path.
    /// “Prefix lookup” means that it should report an aggregate status of all
    /// paths starting with the given prefix (in other words, a directory).
    fn status(&self, index: &Path, prefix_lookup: bool) -> f::Git {
        if prefix_lookup {
            self.dir_status(index)
        } else {
            self.file_status(index)
        }
    }

    /// Get the user-facing status of a file.
    /// We check the statuses directly applying to a file, and for the ignored
    /// status we check if any of its parents directories is ignored by git.
    fn file_status(&self, file: &Path) -> f::Git {
        let path = reorient(file);

        let s = self
            .statuses
            .iter()
            .filter(|p| {
                if p.1 == git2::Status::IGNORED {
                    path.starts_with(&p.0)
                } else {
                    p.0 == path
                }
            })
            .fold(git2::Status::empty(), |a, b| a | b.1);

        let staged = index_status(s);
        let unstaged = working_tree_status(s);
        f::Git { staged, unstaged }
    }

    /// Get the combined, user-facing status of a directory.
    /// Statuses are aggregating (for example, a directory is considered
    /// modified if any file under it has the status modified), except for
    /// ignored status which applies to files under (for example, a directory
    /// is considered ignored if one of its parent directories is ignored).
    fn dir_status(&self, dir: &Path) -> f::Git {
        let path = reorient(dir);

        let s = self
            .statuses
            .iter()
            .filter(|p| {
                if p.1 == git2::Status::IGNORED {
                    path.starts_with(&p.0)
                } else {
                    p.0.starts_with(&path)
                }
            })
            .fold(git2::Status::empty(), |a, b| a | b.1);

        let staged = index_status(s);
        let unstaged = working_tree_status(s);
        f::Git { staged, unstaged }
    }
}

/// Converts a path to an absolute path based on the current directory.
/// Paths need to be absolute for them to be compared properly, otherwise
/// you’d ask a repo about “./README.md” but it only knows about
/// “/vagrant/README.md”, prefixed by the workdir.
#[cfg(unix)]
fn reorient(path: &Path) -> PathBuf {
    use std::env::current_dir;

    // TODO: I’m not 100% on this func tbh
    let path = match current_dir() {
        Err(_) => Path::new(".").join(path),
        Ok(dir) => dir.join(path),
    };

    path.canonicalize().unwrap_or(path)
}

#[cfg(windows)]
fn reorient(path: &Path) -> PathBuf {
    let unc_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // On Windows UNC path is returned. We need to strip the prefix for it to work.
    let normal_path = unc_path
        .as_os_str()
        .to_str()
        .unwrap()
        .trim_start_matches("\\\\?\\");
    PathBuf::from(normal_path)
}

/// The character to display if the file has been modified, but not staged.
fn working_tree_status(status: git2::Status) -> f::GitStatus {
    #[rustfmt::skip]
    return match status {
        s if s.contains(git2::Status::WT_NEW)         => f::GitStatus::New,
        s if s.contains(git2::Status::WT_MODIFIED)    => f::GitStatus::Modified,
        s if s.contains(git2::Status::WT_DELETED)     => f::GitStatus::Deleted,
        s if s.contains(git2::Status::WT_RENAMED)     => f::GitStatus::Renamed,
        s if s.contains(git2::Status::WT_TYPECHANGE)  => f::GitStatus::TypeChange,
        s if s.contains(git2::Status::IGNORED)        => f::GitStatus::Ignored,
        s if s.contains(git2::Status::CONFLICTED)     => f::GitStatus::Conflicted,
        _                                             => f::GitStatus::NotModified,
    };
}

/// The character to display if the file has been modified and the change
/// has been staged.
fn index_status(status: git2::Status) -> f::GitStatus {
    #[rustfmt::skip]
    return match status {
        s if s.contains(git2::Status::INDEX_NEW)         => f::GitStatus::New,
        s if s.contains(git2::Status::INDEX_MODIFIED)    => f::GitStatus::Modified,
        s if s.contains(git2::Status::INDEX_DELETED)     => f::GitStatus::Deleted,
        s if s.contains(git2::Status::INDEX_RENAMED)     => f::GitStatus::Renamed,
        s if s.contains(git2::Status::INDEX_TYPECHANGE)  => f::GitStatus::TypeChange,
        _                                                => f::GitStatus::NotModified,
    };
}

fn current_branch(repo: &git2::Repository) -> Option<String> {
    let head = match repo.head() {
        Ok(head) => Some(head),
        Err(ref e)
            if e.code() == git2::ErrorCode::UnbornBranch
                || e.code() == git2::ErrorCode::NotFound =>
        {
            return None;
        }
        Err(e) => {
            error!("Error looking up Git branch: {e:?}");
            return None;
        }
    };

    head.and_then(|h| h.shorthand().map(std::string::ToString::to_string))
}

impl f::SubdirGitRepo {
    #[must_use]
    pub fn from_path(dir: &Path, status: bool) -> Self {
        let path = &reorient(dir);

        if let Ok(repo) = git2::Repository::open(path) {
            let branch = current_branch(&repo);
            if !status {
                return Self {
                    status: None,
                    branch,
                };
            }
            match repo.statuses(None) {
                Ok(es) => {
                    if es.iter().any(|s| s.status() != git2::Status::IGNORED) {
                        return Self {
                            status: Some(f::SubdirGitRepoStatus::GitDirty),
                            branch,
                        };
                    }
                    return Self {
                        status: Some(f::SubdirGitRepoStatus::GitClean),
                        branch,
                    };
                }
                Err(e) => {
                    error!("Error looking up Git statuses: {e:?}");
                }
            }
        }
        f::SubdirGitRepo {
            status: if status {
                Some(f::SubdirGitRepoStatus::NoRepo)
            } else {
                None
            },
            branch: None,
        }
    }
}

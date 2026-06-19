//! `confine` — the SOLE confinement funnel for all filesystem access.
//!
//! This is the SOLE confinement funnel; every read/save/asset path MUST route
//! through it (audit 04 single-funnel; audit 02 TOCTOU fix). Before this module
//! the funnel was split across `server.rs` (`confine_abs`, `confine_link`) and
//! `asset_origin.rs` (`Confiner::confine`), each re-resolving the same path and
//! each *adding* TOCTOU surface (audit/02-security.md HIGH-2, HIGH-3, MED-4).
//! Track-3 unifies all of it here.
//!
//! ## The invariants this module enforces *by construction*
//!
//! 1. **One funnel, no softer path.** Every caller-supplied path is resolved
//!    here and nowhere else. [`confine_path`] is the lexical/canonical gate;
//!    [`confine_read`], [`confine_save`] and [`confine_link`] build on it.
//!
//! 2. **TOCTOU-free reads (audit HIGH-2 / HIGH-3).** [`confine_read`] does NOT
//!    "confine, then stat, then read" across three separate path syscalls.
//!    Instead it confines the path, then **`open()`s the file exactly once with
//!    `O_NOFOLLOW`**, and **`fstat`s THAT SAME descriptor** for the size cap and
//!    permission mode. The returned [`ConfinedFile`] hands the caller the held
//!    `File` plus the `Metadata` read from that fd — so the route layer (W3)
//!    reads from the held fd and applies the permission floor to the *fstat'd*
//!    metadata. There is no second `stat`, no re-`open`, and the final component
//!    can never be a followed symlink.
//!
//! 3. **Symlink-safe saves (audit HIGH-2).** [`confine_save`] never writes
//!    through a possibly-symlinked path with `fs::write`. It writes a temp file
//!    in the *same directory* using `O_NOFOLLOW | O_EXCL` (so a pre-planted
//!    symlink at the temp name is refused, not followed) and then `rename(2)`s
//!    it over the target. A symlink swap of the final component cannot redirect
//!    the write outside the directory.
//!
//! 4. **Denylist always applies (audit MED-4).** Sensitive / denylisted paths
//!    are rejected even on the empty-registry fallback. `roots::is_sensitive`
//!    is consulted on *every* path regardless of whether [`Roots::union`] is
//!    empty.
//!
//! ## What this module does NOT do
//! It does **not** apply the authentication permission floor. [`confine_read`]
//! returns the fstat'd [`std::fs::Metadata`] so the route layer (W3) can call
//! `auth::floor_allows(auth::mode_of(&meta), authenticated)` on the *same* fd's
//! metadata. Keeping the floor out of the funnel keeps the funnel free of the
//! auth-state dependency while still giving the route the held-fd metadata it
//! needs to honour "never serve what the caller couldn't already read".
//!
//! No `unwrap` / `expect` / `panic` in production code paths.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::roots::Root;

/// Default maximum size, in bytes, that [`confine_read`] will accept for a file.
///
/// The cap is checked against the length read from the *held fd's* `fstat`, so a
/// pathological multi-GB file is refused before the caller streams it. 8 MiB
/// comfortably covers any realistic Markdown document; mirrors
/// [`crate::file_peer::DEFAULT_MAX_FILE_SIZE`].
pub const DEFAULT_MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;

/// Why a confinement attempt was refused. Mapped onto HTTP / CLI responses by
/// the route layer; none of these variants ever panic.
#[derive(Debug)]
pub enum ConfineError {
    /// The requested path was not absolute. Confinement only reasons about
    /// absolute paths; relative input is a caller bug, not an escape attempt.
    NotAbsolute(PathBuf),
    /// The resolved path lies outside every active root (a traversal / symlink
    /// escape, or simply an un-registered location).
    Escapes(PathBuf),
    /// The path is on the sensitive denylist (`roots::is_sensitive`): `$HOME`
    /// itself, `~/.ssh`, `~/.gnupg`, `~/.aws`, `/etc`, … Refused even when the
    /// registry is empty (audit MED-4).
    Sensitive(PathBuf),
    /// The fstat'd file exceeds the size cap. Carries the actual and maximum
    /// sizes in bytes.
    TooLarge { size: u64, max: u64 },
    /// An underlying filesystem error (open / fstat / rename / write). The
    /// final component being a symlink surfaces here as `O_NOFOLLOW` makes
    /// `open` fail with `ELOOP`.
    Io(std::io::Error),
}

impl std::fmt::Display for ConfineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfineError::NotAbsolute(p) => {
                write!(f, "path is not absolute: {}", p.display())
            }
            ConfineError::Escapes(p) => {
                write!(f, "path escapes the active roots: {}", p.display())
            }
            ConfineError::Sensitive(p) => {
                write!(f, "path is on the sensitive denylist: {}", p.display())
            }
            ConfineError::TooLarge { size, max } => {
                write!(f, "file is {size} bytes, exceeds max of {max} bytes")
            }
            ConfineError::Io(e) => write!(f, "filesystem error during confinement: {e}"),
        }
    }
}

impl std::error::Error for ConfineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfineError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfineError {
    fn from(e: std::io::Error) -> Self {
        ConfineError::Io(e)
    }
}

/// A file opened through the funnel, with the metadata read from the *same* fd.
///
/// Returned by [`confine_read`]. The whole point of this type is that
/// [`Self::file`] and [`Self::metadata`] describe the **identical open
/// descriptor**: the caller reads bytes from `file` and applies the permission
/// floor to `metadata`, with no intervening `stat` or re-`open` (audit HIGH-2 /
/// HIGH-3). `canonical` is the confined path for logging / link rewriting.
#[derive(Debug)]
pub struct ConfinedFile {
    /// The held descriptor, opened once with `O_NOFOLLOW`. Read document bytes
    /// from here — never re-open `canonical`.
    pub file: std::fs::File,
    /// Metadata `fstat`'d from [`Self::file`]'s descriptor. Feed this to
    /// `auth::floor_allows(auth::mode_of(&metadata), authenticated)` in the
    /// route layer; the floor is intentionally not applied inside the funnel.
    pub metadata: std::fs::Metadata,
    /// The canonical, in-root path that was opened.
    pub canonical: PathBuf,
}

/// How [`confine_link`] classified a document link target.
///
/// Unlike the read/save funnels, link rewriting never errors on an escape — it
/// *classifies* it, so the renderer can emit the `/outside` 403 marker rather
/// than a broken link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkResolution {
    /// The link resolves to a path inside an active root; carries its canonical
    /// path (what the rewritten URL should point at).
    InRoot(PathBuf),
    /// The link escapes every root (traversal, symlink-out, sensitive, or
    /// otherwise un-served). The route layer renders this as the `/outside`
    /// 403 sentinel.
    Outside,
}

/// Strict-descent containment check: is `candidate` `root` itself, or a strict
/// descendant of it?
///
/// Both paths are expected to be canonical (no `..`, no symlinks). Uses
/// component-wise `starts_with`, which — unlike a string prefix — will not treat
/// `/a/bc` as living under `/a/b`. This mirrors the primitive in
/// `file_peer::resolve_within` so the whole funnel agrees on what "contained"
/// means.
fn is_within(candidate: &Path, root: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

/// Does any active root in `roots_union` own `canonical`?
///
/// Honours each root's [`crate::roots::RootKind`]: a `SingleFile` root owns only
/// its exact path; a `Directory` root owns itself and any descendant.
/// `canonical` must already be canonicalized.
fn owned_by_any(canonical: &Path, roots_union: &[&Root]) -> bool {
    roots_union.iter().any(|root| match root.kind {
        crate::roots::RootKind::SingleFile => root.path == canonical,
        crate::roots::RootKind::Directory => is_within(canonical, &root.path),
    })
}

/// Canonicalize an existing `requested` absolute path and confirm it is both
/// contained within the active roots and not on the sensitive denylist.
///
/// This is the lexical/canonical gate every other entry point builds on. It
/// resolves symlinks via [`Path::canonicalize`] (so a symlink whose target lies
/// outside the roots is rejected here), then requires the canonical path to be
/// owned by some root in `roots_union`.
///
/// The denylist ([`Roots::is_sensitive`](crate::roots::Roots::is_sensitive)) is
/// checked **unconditionally** — including when `roots_union` is empty — so the
/// empty-registry fallback can never serve `/etc/...` or `~/.ssh/...` (audit
/// MED-4). The caller passes a [`Roots`](crate::roots::Roots) reference solely to
/// consult that denylist; the *containment* decision uses `roots_union`.
///
/// On success returns the canonical, in-root path. The file must exist (it is
/// canonicalized); for a not-yet-existing save target use [`confine_save`],
/// which confines the parent directory instead.
pub fn confine_path(
    requested: &Path,
    roots_union: &[&Root],
    registry: &crate::roots::Roots,
) -> Result<PathBuf, ConfineError> {
    if !requested.is_absolute() {
        return Err(ConfineError::NotAbsolute(requested.to_path_buf()));
    }

    // Resolve symlinks and `..`. A symlink whose target lies outside the roots
    // is neutralised here: canonicalize follows it, and the result then fails
    // the containment check below.
    let canonical = requested.canonicalize().map_err(ConfineError::Io)?;

    // Denylist FIRST, and unconditionally (audit MED-4): even an empty registry
    // must refuse sensitive targets.
    if registry.is_sensitive(&canonical) {
        return Err(ConfineError::Sensitive(canonical));
    }

    if !owned_by_any(&canonical, roots_union) {
        return Err(ConfineError::Escapes(canonical));
    }

    Ok(canonical)
}

/// The TOCTOU-free read path (audit HIGH-2 / HIGH-3).
///
/// Confines `requested` via [`confine_path`], then **opens the canonical path
/// exactly once with `O_NOFOLLOW`** and **`fstat`s that same descriptor** for
/// the size cap and the permission mode. The returned [`ConfinedFile`] lets the
/// caller read from the held fd and apply the permission floor to the fstat'd
/// metadata — there is no second `stat` and no re-`open`, so a symlink swapped
/// in after confinement cannot redirect the read, and the final component can
/// never itself be a followed symlink.
///
/// The size cap is enforced from the **fstat'd length** (not a prior path
/// `stat`). Pass [`DEFAULT_MAX_FILE_SIZE`] for the standard cap.
///
/// The auth floor is intentionally NOT applied here; the route layer calls
/// `auth::floor_allows(auth::mode_of(&confined.metadata), authenticated)`.
pub fn confine_read(
    requested: &Path,
    roots_union: &[&Root],
    registry: &crate::roots::Roots,
    max_file_size: u64,
) -> Result<ConfinedFile, ConfineError> {
    let canonical = confine_path(requested, roots_union, registry)?;

    // Open ONCE with O_NOFOLLOW. If the (canonical) final component is a
    // symlink — e.g. swapped in after canonicalize — `open` fails with ELOOP
    // rather than following it.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&canonical)
        .map_err(ConfineError::Io)?;

    // fstat the SAME fd. No second path stat: the metadata describes exactly
    // the bytes we will serve.
    let metadata = file.metadata().map_err(ConfineError::Io)?;

    // Refuse non-regular files (a directory, fifo, device, …): only a real file
    // can be served as a document, and this blocks reading from a non-symlink
    // special that slipped through.
    if !metadata.is_file() {
        return Err(ConfineError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "confined target is not a regular file",
        )));
    }

    // Size cap from the held fd's length.
    let size = metadata.len();
    if size > max_file_size {
        return Err(ConfineError::TooLarge {
            size,
            max: max_file_size,
        });
    }

    Ok(ConfinedFile {
        file,
        metadata,
        canonical,
    })
}

/// Atomically write `bytes` to `requested`, confined and without following a
/// symlinked final component (audit HIGH-2).
///
/// The target file need not exist yet, so we cannot canonicalize it directly.
/// Instead the **parent directory** is canonicalized and confined (it must be
/// in-root and not sensitive), the final component is taken verbatim, and the
/// write goes to a temp file created in that same directory with
/// `O_NOFOLLOW | O_EXCL` — so a symlink pre-planted at the temp name is refused
/// (`O_EXCL`) rather than followed (`O_NOFOLLOW`). The temp is then
/// `rename(2)`'d over the target. A symlink swap of the target's final component
/// cannot redirect the write outside the (confined) directory, because `rename`
/// operates on the directory entry, not on a followed link.
pub fn confine_save(
    requested: &Path,
    roots_union: &[&Root],
    registry: &crate::roots::Roots,
    bytes: &[u8],
) -> Result<(), ConfineError> {
    if !requested.is_absolute() {
        return Err(ConfineError::NotAbsolute(requested.to_path_buf()));
    }

    let parent = requested
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| ConfineError::Escapes(requested.to_path_buf()))?;
    let file_name = requested
        .file_name()
        .ok_or_else(|| ConfineError::Escapes(requested.to_path_buf()))?;

    // Confine the parent directory (it must exist). This canonicalizes it,
    // applies the denylist, and confirms containment.
    let canonical_parent = confine_path(parent, roots_union, registry)?;
    let canonical_target = canonical_parent.join(file_name);
    // Denylist the resolved target too (e.g. a save to `~/.ssh/x`).
    if registry.is_sensitive(&canonical_target) {
        return Err(ConfineError::Sensitive(canonical_target));
    }

    // Temp file in the SAME directory (so rename is atomic / same filesystem).
    // O_EXCL refuses a pre-existing entry at the temp name (incl. a planted
    // symlink); O_NOFOLLOW refuses to follow one if it raced in.
    let temp_name = temp_sibling_name(file_name);
    let temp_path = canonical_parent.join(&temp_name);

    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_EXCL)
            .open(&temp_path)?;
        temp.write_all(bytes)?;
        temp.sync_all()?;
        // Atomically replace the target. rename(2) acts on the directory entry,
        // so it does not follow a symlinked target — the bytes land in the
        // confined directory regardless of a swapped final component.
        std::fs::rename(&temp_path, &canonical_target)
    })();

    if let Err(e) = write_result {
        // Best-effort cleanup of the temp file; ignore its own error.
        let _ = std::fs::remove_file(&temp_path);
        return Err(ConfineError::Io(e));
    }

    Ok(())
}

/// Build a sibling temp-file name for an atomic save: `.<name>.<pid>.<n>.tmp`.
///
/// Hidden (leading dot) and process+counter qualified so concurrent saves in
/// the same directory don't collide. `O_EXCL` is the real guard; this just keeps
/// collisions rare.
fn temp_sibling_name(file_name: &std::ffi::OsStr) -> std::ffi::OsString {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut name = std::ffi::OsString::from(".");
    name.push(file_name);
    name.push(format!(".{}.{}.tmp", std::process::id(), n));
    name
}

/// Classify a document-link target for rewriting.
///
/// Unlike [`confine_read`], this never errors on an escape — it returns
/// [`LinkResolution::Outside`] so the renderer emits the `/outside` 403 marker
/// instead of a dead link. A non-absolute path, a missing target, a sensitive
/// path, or any escape all map to `Outside`; only a path that canonicalizes
/// in-root and off the denylist yields [`LinkResolution::InRoot`].
pub fn confine_link(
    requested: &Path,
    roots_union: &[&Root],
    registry: &crate::roots::Roots,
) -> LinkResolution {
    match confine_path(requested, roots_union, registry) {
        Ok(canonical) => LinkResolution::InRoot(canonical),
        Err(_) => LinkResolution::Outside,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roots::{RootKind, Roots};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    /// A unique temp directory for one test, cleaned up on drop. Avoids an
    /// external tempfile dependency (mirrors `file_peer`'s helper).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("md-preview-confine-{}-{}-{}", tag, std::process::id(), n));
            std::fs::create_dir_all(&path).expect("create temp dir");
            // Canonicalize so comparisons against canonicalized results hold
            // even when $TMPDIR is itself a symlink (e.g. /tmp -> /private/tmp).
            let path = path.canonicalize().expect("canonicalize temp dir");
            Self { path }
        }

        fn file(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A directory `Root` over `path`, stamped now.
    fn dir_root(path: &Path) -> Root {
        Root {
            kind: RootKind::Directory,
            path: path.to_path_buf(),
            last_used: SystemTime::now(),
        }
    }

    /// A registry whose `home` is set so the denylist is meaningful.
    fn registry_home(home: &Path) -> Roots {
        Roots::new(home.to_path_buf())
    }

    // --- confine_path -----------------------------------------------------

    #[test]
    fn confine_path_accepts_in_root_file() {
        let dir = TempDir::new("path-ok");
        let file = dir.file("doc.md");
        std::fs::write(&file, "hi").unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        let canon = confine_path(&file, &union, &reg).expect("in-root file confines");
        assert_eq!(canon, file);
    }

    #[test]
    fn confine_path_rejects_non_absolute() {
        let dir = TempDir::new("path-rel");
        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        let err = confine_path(Path::new("relative/doc.md"), &union, &reg)
            .expect_err("relative path rejected");
        assert!(matches!(err, ConfineError::NotAbsolute(_)));
    }

    #[test]
    fn confine_path_rejects_parent_traversal_escape() {
        // root/sub is the root; root/secret.md is outside it.
        let dir = TempDir::new("path-trav");
        let sub = dir.file("sub");
        std::fs::create_dir(&sub).unwrap();
        let secret = dir.file("secret.md");
        std::fs::write(&secret, "secret").unwrap();

        let root = dir_root(&sub);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        // A `..` traversal that climbs out of `sub` to `secret.md`.
        let escaping = sub.join("..").join("secret.md");
        let err =
            confine_path(&escaping, &union, &reg).expect_err("traversal out of root rejected");
        assert!(matches!(err, ConfineError::Escapes(_)));
    }

    #[test]
    fn confine_path_rejects_symlink_escaping_root() {
        // A symlink INSIDE the root pointing at a file OUTSIDE it must be
        // rejected: canonicalize resolves the link, and the result is out-of-root.
        let dir = TempDir::new("path-sym");
        let outside = TempDir::new("path-sym-out");
        let secret = outside.file("secret.md");
        std::fs::write(&secret, "top secret").unwrap();

        let link = dir.file("link.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        let err =
            confine_path(&link, &union, &reg).expect_err("symlink escaping root rejected");
        assert!(matches!(err, ConfineError::Escapes(_)));
    }

    #[test]
    fn confine_path_rejects_sensitive_even_with_empty_registry() {
        // audit MED-4: the denylist applies with an EMPTY union.
        let home = TempDir::new("path-sens-home");
        let ssh = home.file(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let key = ssh.join("id_rsa");
        std::fs::write(&key, "PRIVATE KEY").unwrap();

        let reg = registry_home(&home.path);
        // Empty union — yet the sensitive path must still be refused.
        let union: Vec<&Root> = vec![];

        let err = confine_path(&key, &union, &reg)
            .expect_err("sensitive path rejected even with empty registry");
        assert!(matches!(err, ConfineError::Sensitive(_)));
    }

    #[test]
    fn confine_path_rejects_out_of_root_when_registry_empty() {
        let dir = TempDir::new("path-empty");
        let file = dir.file("doc.md");
        std::fs::write(&file, "hi").unwrap();

        let reg = registry_home(Path::new("/nonexistent-home"));
        let union: Vec<&Root> = vec![];

        let err = confine_path(&file, &union, &reg).expect_err("no roots own this path");
        assert!(matches!(err, ConfineError::Escapes(_)));
    }

    #[test]
    fn confine_path_single_file_root_owns_only_exact_path() {
        let dir = TempDir::new("path-single");
        let served = dir.file("served.md");
        let other = dir.file("other.md");
        std::fs::write(&served, "a").unwrap();
        std::fs::write(&other, "b").unwrap();

        let root = Root {
            kind: RootKind::SingleFile,
            path: served.clone(),
            last_used: SystemTime::now(),
        };
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        assert!(confine_path(&served, &union, &reg).is_ok());
        let err = confine_path(&other, &union, &reg)
            .expect_err("single-file root must not own a sibling");
        assert!(matches!(err, ConfineError::Escapes(_)));
    }

    // --- confine_read -----------------------------------------------------

    #[test]
    fn confine_read_returns_held_fd_and_fstat_metadata() {
        let dir = TempDir::new("read-ok");
        let file = dir.file("doc.md");
        std::fs::write(&file, "hello world").unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        let mut confined =
            confine_read(&file, &union, &reg, DEFAULT_MAX_FILE_SIZE).expect("read confines");
        assert_eq!(confined.canonical, file);
        assert_eq!(confined.metadata.len(), 11);
        assert!(confined.metadata.is_file());

        // The held fd reads the actual bytes (no re-open).
        use std::io::Read;
        let mut buf = String::new();
        confined.file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello world");
    }

    #[test]
    fn confine_read_size_cap_from_held_fd() {
        let dir = TempDir::new("read-cap");
        let file = dir.file("doc.md");
        std::fs::write(&file, "0123456789abcdef").unwrap(); // 16 bytes

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        // Under the cap: ok.
        assert!(confine_read(&file, &union, &reg, 16).is_ok());

        // Over the cap (max 8): refused via the fstat'd length.
        let err = confine_read(&file, &union, &reg, 8).expect_err("over-cap file refused");
        match err {
            ConfineError::TooLarge { size, max } => {
                assert_eq!(size, 16);
                assert_eq!(max, 8);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn confine_read_does_not_follow_symlink_final_component() {
        // O_NOFOLLOW proof: an IN-ROOT symlink pointing at an IN-ROOT file must
        // not be openable via the link path — the funnel's open never follows a
        // final-component symlink (the swap-after-confine attack).
        let dir = TempDir::new("read-nofollow");
        let real = dir.file("real.md");
        std::fs::write(&real, "real contents").unwrap();

        let link = dir.file("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        // Opening the canonical path (real.md) is fine.
        assert!(confine_read(&real, &union, &reg, DEFAULT_MAX_FILE_SIZE).is_ok());

        // Directly O_NOFOLLOW-opening the symlink path fails — proves the
        // funnel's open never follows a final-component symlink.
        let direct = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&link);
        assert!(
            direct.is_err(),
            "O_NOFOLLOW must refuse a symlink final component"
        );
    }

    #[test]
    fn confine_read_rejects_symlink_escaping_root() {
        let dir = TempDir::new("read-sym-esc");
        let outside = TempDir::new("read-sym-esc-out");
        let secret = outside.file("secret.md");
        std::fs::write(&secret, "secret").unwrap();
        let link = dir.file("link.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        let err = confine_read(&link, &union, &reg, DEFAULT_MAX_FILE_SIZE)
            .expect_err("escaping symlink refused");
        assert!(matches!(err, ConfineError::Escapes(_)));
    }

    // --- confine_save -----------------------------------------------------

    #[test]
    fn confine_save_writes_atomically_in_root() {
        let dir = TempDir::new("save-ok");
        let file = dir.file("doc.md");

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        confine_save(&file, &union, &reg, b"new contents").expect("save confines");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new contents");

        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir.path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file must be renamed away");

        // Overwrites cleanly a second time.
        confine_save(&file, &union, &reg, b"second").expect("re-save confines");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "second");
    }

    #[test]
    fn confine_save_does_not_follow_symlinked_target() {
        // The save target is a symlink pointing OUTSIDE the root. The atomic
        // rename must NOT write through the link to the outside file; instead it
        // replaces the link's directory entry with a real in-root file.
        let dir = TempDir::new("save-sym");
        let outside = TempDir::new("save-sym-out");
        let victim = outside.file("victim.md");
        std::fs::write(&victim, "ORIGINAL").unwrap();

        let target = dir.file("doc.md");
        std::os::unix::fs::symlink(&victim, &target).unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        confine_save(&target, &union, &reg, b"SAFE").expect("save through symlink target");

        // The outside victim is UNTOUCHED — the write did not follow the link.
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "ORIGINAL");

        // The in-root entry is now a real file with the new bytes (rename
        // replaced the symlink entry).
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(
            meta.file_type().is_file(),
            "target is now a real file, not a symlink"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "SAFE");
    }

    #[test]
    fn confine_save_rejects_out_of_root_parent() {
        let outside = TempDir::new("save-out");
        let dir = TempDir::new("save-out-root");
        let target = outside.file("doc.md");

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        let err = confine_save(&target, &union, &reg, b"x")
            .expect_err("save outside the root rejected");
        assert!(matches!(err, ConfineError::Escapes(_)));
    }

    #[test]
    fn confine_save_rejects_non_absolute() {
        let dir = TempDir::new("save-rel");
        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        let err = confine_save(Path::new("rel.md"), &union, &reg, b"x")
            .expect_err("relative save rejected");
        assert!(matches!(err, ConfineError::NotAbsolute(_)));
    }

    // --- confine_link -----------------------------------------------------

    #[test]
    fn confine_link_classifies_in_root() {
        let dir = TempDir::new("link-in");
        let file = dir.file("doc.md");
        std::fs::write(&file, "x").unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        match confine_link(&file, &union, &reg) {
            LinkResolution::InRoot(p) => assert_eq!(p, file),
            LinkResolution::Outside => panic!("in-root link misclassified as Outside"),
        }
    }

    #[test]
    fn confine_link_classifies_out_of_root_as_outside() {
        let dir = TempDir::new("link-out-root");
        let outside = TempDir::new("link-out");
        let file = outside.file("doc.md");
        std::fs::write(&file, "x").unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        assert_eq!(confine_link(&file, &union, &reg), LinkResolution::Outside);
    }

    #[test]
    fn confine_link_classifies_symlink_escape_as_outside() {
        let dir = TempDir::new("link-sym-root");
        let outside = TempDir::new("link-sym-out");
        let secret = outside.file("secret.md");
        std::fs::write(&secret, "x").unwrap();
        let link = dir.file("link.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let root = dir_root(&dir.path);
        let union = vec![&root];
        let reg = registry_home(Path::new("/nonexistent-home"));

        assert_eq!(confine_link(&link, &union, &reg), LinkResolution::Outside);
    }
}

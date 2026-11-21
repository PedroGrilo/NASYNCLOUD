//! Desktop, Documents and Pictures on the NAS, the way OneDrive's "folder
//! backup" (Known Folder Move) does it: what is in the folder moves into the
//! sync folder (`<root>\Documentos`...), and Windows is told that the known
//! folder now lives there (`SHSetKnownFolderPath`). Explorer, "Save as" and
//! every program keep finding it under the same name, while the files are
//! uploaded and can be freed like any other.
//!
//! Undoing it points the known folder back to its default place, empty; the
//! files stay on the NAS (as OneDrive does).

use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use nasyncloud::remote::{self, Entry, Version};
use serde::Serialize;
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    SHChangeNotify, SHGetKnownFolderPath, SHSetKnownFolderPath, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Pictures, KF_FLAG_DEFAULT_PATH, KF_FLAG_DONT_VERIFY, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
};

use super::{clean, create_info, inspect, refusal, unix_now, Engine};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

struct Known {
    key: &'static str,
    /// Name on the NAS (inside the sync folder), and what the interface calls it.
    name: &'static str,
    id: GUID,
}

const KNOWN: [Known; 3] = [
    Known { key: "desktop", name: "Ambiente de trabalho", id: FOLDERID_Desktop },
    Known { key: "documents", name: "Documentos", id: FOLDERID_Documents },
    Known { key: "pictures", name: "Imagens", id: FOLDERID_Pictures },
];

fn known(key: &str) -> Result<&'static Known> {
    KNOWN.iter().find(|k| k.key == key).ok_or_else(|| anyhow!("unknown folder {key:?}"))
}

/// One known folder, for the interface.
#[derive(Clone, Debug, Serialize)]
pub struct FolderView {
    pub key: String,
    pub name: String,
    /// Where Windows has it now.
    pub path: String,
    /// Whether that is inside the sync folder.
    pub on_nas: bool,
    /// The folder of the NAS it is in (`""` is the top), when `on_nas`.
    pub place: String,
    /// It is in the sync folder, but the NAS refused to create it there, so
    /// nothing in it can be sent: it has to go somewhere else.
    pub refused: bool,
}

/// The known folders, and where on the NAS they can go.
#[derive(Clone, Debug, Serialize)]
pub struct Folders {
    pub folders: Vec<FolderView>,
    /// The sync folder.
    pub root: String,
    /// Folders of the NAS to choose from (`""` is the top). The top is left
    /// out once the NAS has refused a folder there: with the address of the
    /// NAS itself (`http://nas:5005/`), UGOS shows the list of shared folders,
    /// and only inside one of them can anything be created.
    pub places: Vec<String>,
    /// The one to suggest, when there is an obvious one.
    pub place: Option<String>,
}

/// What moving a folder did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MoveReport {
    pub moved: usize,
    /// Items that could not be moved (they stay in `from`), with the reason.
    pub left: Vec<String>,
    pub from: String,
    pub to: String,
    /// The NAS does not allow the folder in that place: nothing was moved.
    pub refused: bool,
}

fn known_path(id: &GUID, flags: i32) -> Result<PathBuf> {
    let mut p: PWSTR = std::ptr::null_mut();
    // SAFETY: plain out-parameter call; the buffer is freed below either way.
    let hr = unsafe { SHGetKnownFolderPath(id, flags as u32, std::ptr::null_mut(), &mut p) };
    let out = if hr >= 0 && !p.is_null() {
        // SAFETY: on success `p` is a NUL-terminated wide string.
        let wide = unsafe {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            std::slice::from_raw_parts(p, len)
        };
        Ok(PathBuf::from(OsString::from_wide(wide)))
    } else {
        Err(anyhow!("SHGetKnownFolderPath failed (0x{:08x})", hr as u32))
    };
    // SAFETY: CoTaskMemFree accepts null.
    unsafe { CoTaskMemFree(p as *const _) };
    out
}

fn set_known_path(id: &GUID, path: &Path) -> Result<()> {
    let wide: Vec<u16> = Path::new(&clean(path)).as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let hr = unsafe { SHSetKnownFolderPath(id, 0, std::ptr::null_mut(), wide.as_ptr()) };
    if hr < 0 {
        bail!("Windows refused to move the folder to {} (0x{:08x})", path.display(), hr as u32);
    }
    // Explorer redraws the desktop and the folder shortcuts.
    // SAFETY: no item pointers are needed for SHCNE_ASSOCCHANGED.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    Ok(())
}

/// Whether `path` is `root` or inside it.
fn under(root: &Path, path: &Path) -> bool {
    let (root, path) = (clean(root).to_lowercase(), clean(path).to_lowercase());
    path == root || path.strip_prefix(&root).is_some_and(|rest| rest.starts_with('\\'))
}

/// Whether the relative path `rel` is `dir` or inside it.
fn within(dir: &str, rel: &str) -> bool {
    let (dir, rel) = (dir.to_lowercase(), rel.to_lowercase());
    rel == dir || rel.strip_prefix(&dir).is_some_and(|rest| rest.starts_with('/'))
}

/// Something with a reparse point in it (a OneDrive file, a link) is copied
/// rather than renamed: a rename would carry that other mechanism along.
fn has_reparse(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else { return true };
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return true;
    }
    meta.is_dir() && fs::read_dir(path).map_or(true, |it| it.flatten().any(|e| has_reparse(&e.path())))
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return Ok(()); // links (and junctions) stay behind
    }
    if meta.is_dir() {
        fs::create_dir(to).with_context(|| format!("cannot create {}", to.display()))?;
        for e in fs::read_dir(from)?.flatten() {
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
    } else {
        // Reading a file that is only in OneDrive's cloud downloads it first.
        fs::copy(from, to).with_context(|| format!("cannot copy {}", from.display()))?;
    }
    Ok(())
}

fn move_item(from: &Path, to: &Path) -> Result<()> {
    if !has_reparse(from) && fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if let Err(e) = copy_tree(from, to) {
        // Half a copy would be uploaded as if it were the whole thing.
        let _ = if to.is_dir() { fs::remove_dir_all(to) } else { fs::remove_file(to) };
        return Err(e);
    }
    if from.is_dir() { fs::remove_dir_all(from) } else { fs::remove_file(from) }.with_context(|| format!("copied, but cannot remove {}", from.display()))
}

impl Engine {
    pub(super) fn folders(&self) -> Result<Folders> {
        let refused = self.refused.lock().unwrap().clone();
        let mut folders = Vec::new();
        let mut taken = Vec::new();
        for k in &KNOWN {
            let path = known_path(&k.id, KF_FLAG_DONT_VERIFY)?;
            let rel = self.rel(&path).filter(|r| !r.is_empty());
            taken.extend(rel.clone());
            folders.push(FolderView {
                key: k.key.into(),
                name: k.name.into(),
                on_nas: under(&self.root, &path),
                path: clean(&path),
                place: rel.as_deref().map(|r| remote::split(r).0.to_string()).unwrap_or_default(),
                refused: rel.is_some_and(|r| refused.contains(&r.to_lowercase())),
            });
        }
        // Where they can go: the top (unless the NAS refused a folder there)
        // and the folders the NAS has at the top.
        let mut places = Vec::new();
        if !self.top_closed.load(Ordering::SeqCst) {
            places.push(String::new());
        }
        let mut tops: Vec<String> = self
            .listing(|| fs::read_dir(&self.root).map(|it| it.flatten().collect::<Vec<_>>()))?
            .into_iter()
            .filter(|e| e.metadata().is_ok_and(|m| m.is_dir()))
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            // Not one of these folders itself, nor a folder that only exists here.
            .filter(|(name, path)| !taken.iter().any(|t| within(t, name)) && !self.ignored(name, true) && inspect(path, 0).is_ok_and(|l| l.placeholder))
            .map(|(name, _)| name)
            .collect();
        tops.sort_by_key(|n| n.to_lowercase());
        places.extend(tops);
        // Next to the ones already there; otherwise the top, where it can be used.
        let place = folders.iter().find(|f| f.on_nas && !f.refused && places.contains(&f.place)).map(|f| f.place.clone()).or_else(|| places.first().filter(|p| p.is_empty() || places.len() == 1).cloned());
        Ok(Folders { folders, root: self.root_text.clone(), places, place })
    }

    /// Moves what is in the known folder into `place` (a folder of the NAS,
    /// `""` for the top) in the sync folder and points Windows at it there.
    /// A known folder that is already in the sync folder moves the same way:
    /// that is how one gets out of a place where the NAS refused it.
    pub(super) fn move_folder(&self, key: &str, place: &str) -> Result<MoveReport> {
        let k = known(key)?;
        let from = known_path(&k.id, KF_FLAG_DONT_VERIFY)?;
        let rel = remote::join(place, k.name);
        let to = self.abs(&rel);
        let mut report = MoveReport { from: clean(&from), to: clean(&to), ..Default::default() };
        if report.from.to_lowercase() == report.to.to_lowercase() {
            return Ok(report); // already there
        }
        for other in &KNOWN {
            let at = known_path(&other.id, KF_FLAG_DONT_VERIFY)?;
            if self.rel(&at).filter(|r| !r.is_empty()).is_some_and(|r| within(&r, place)) {
                bail!("{place} is (in) the folder {}: choose another place", other.name);
            }
        }
        // Windows must know what the NAS has in there before anything goes in,
        // or a name could exist twice.
        self.listing(|| fs::read_dir(&self.root).map(|it| it.count()))?;
        if !place.is_empty() {
            let dir = self.abs(place);
            if !inspect(&dir, 0).is_ok_and(|l| l.placeholder) || !dir.is_dir() {
                bail!("{place} is not a folder of the NAS");
            }
            self.listing(|| fs::read_dir(&dir).map(|it| it.count()))?;
        }
        if fs::symlink_metadata(&to).is_ok_and(|m| !m.is_dir()) {
            bail!("{} already exists and is not a folder", to.display());
        }
        // The NAS gets the folder before anything moves. Where it refuses
        // (at the top of `http://nas:5005/`, UGOS only has the list of shared
        // folders and answers 405), nothing has been touched yet.
        if let Err(e) = self.net(async {
            self.ensure_parents(&rel).await?;
            self.make_dir(&rel).await
        }) {
            if !refusal(&e) {
                bail!("cannot create {rel} on the NAS: {e}");
            }
            log::warn!("{e}; nothing was moved");
            report.refused = true;
            return Ok(report);
        }
        if !to.is_dir() {
            // A synced folder from the start (the NAS has it already), so that
            // nothing has to be converted once Windows uses it: Explorer
            // never lets go of the Desktop.
            let entry = Entry { name: k.name.to_string(), is_dir: true, version: Version { mtime: unix_now(), ..Default::default() } };
            if let Err(e) = create_info(&entry).create(self.abs(place)) {
                // (Unless the worker saw it on the NAS first.)
                if !to.is_dir() {
                    log::warn!("could not create {} as a synced folder ({e}); creating an ordinary one", to.display());
                    fs::create_dir(&to).with_context(|| format!("cannot create {}", to.display()))?;
                }
            }
        }
        if inspect(&to, 0).is_ok_and(|l| !l.placeholder) {
            self.enqueue(to.clone(), Duration::ZERO); // the worker adopts it: it is on the NAS already
        }
        self.listing(|| fs::read_dir(&to).map(|it| it.count()))?;
        // Leaving a place inside the sync folder (one the NAS refused).
        let leaving = self.rel(&from).filter(|r| !r.is_empty());
        self.move_items(&from, &to, leaving.is_some(), true, &mut report);
        set_known_path(&k.id, &to)?;
        log::info!("{} is now on the NAS, in {} ({} item(s) moved, {} left in {})", k.name, to.display(), report.moved, report.left.len(), from.display());
        if let Some(old) = leaving {
            self.leave(&from, &old);
        }
        Ok(report)
    }

    /// Moves what is in `from` into `to`, counting in `report`. `tidy` also
    /// removes the shell's own litter from `from` (when that folder is ours to
    /// remove afterwards); `upload` queues what arrives to be sent.
    fn move_items(&self, from: &Path, to: &Path, tidy: bool, upload: bool, report: &mut MoveReport) {
        let items = match fs::read_dir(from) {
            Ok(items) => items,
            Err(e) => {
                if from.exists() {
                    log::warn!("cannot read {}: {e}", from.display());
                    report.left.push(format!("{}: {e}", from.display()));
                }
                return;
            }
        };
        for item in items.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let src = item.path();
            // `desktop.ini` gives the old folder its translated name; links
            // like "My Music" in Documents point elsewhere: all stay.
            if matches!(name.to_ascii_lowercase().as_str(), "desktop.ini" | "thumbs.db") {
                if tidy {
                    let _ = fs::remove_file(&src);
                }
                continue;
            }
            if fs::symlink_metadata(&src).is_ok_and(|m| m.file_type().is_symlink()) {
                continue;
            }
            let mut dest = to.join(&name);
            if fs::symlink_metadata(&dest).is_ok() {
                dest = to.join(remote::conflict_name(&name, unix_now(), |c| to.join(c).exists()));
            }
            match move_item(&src, &dest) {
                Ok(()) => {
                    report.moved += 1;
                    if upload {
                        self.enqueue(dest, Duration::ZERO);
                    }
                }
                Err(e) => {
                    log::warn!("could not move {}: {e:#}", src.display());
                    report.left.push(format!("{name}: {e:#}"));
                }
            }
        }
    }

    /// A known folder has left `from` (`rel` in the sync folder), typically a
    /// place where it never made it to the NAS: the folder goes away from
    /// here, with what was queued or had failed for it (nothing is deleted on
    /// the NAS). Anything that could not be moved keeps it here (and is in
    /// the report).
    fn leave(&self, from: &Path, rel: &str) {
        self.refused.lock().unwrap().remove(&rel.to_lowercase());
        self.queue.lock().unwrap().retain(|p, _| !under(from, p));
        self.failures.lock().unwrap().retain(|p, _| !under(from, p));
        if let Err(e) = self.as_ours(&[rel], || fs::remove_dir(from)) {
            log::info!("{} stays, not empty yet: {e}", from.display());
        }
    }

    /// Points the known folder back at its default place. The files stay on
    /// the NAS (as OneDrive does), so that place is empty; unless the NAS had
    /// refused the folder (`refused` in the report): then they never got
    /// there, and go back with it.
    pub(super) fn restore_folder(&self, key: &str) -> Result<MoveReport> {
        let k = known(key)?;
        let now = known_path(&k.id, KF_FLAG_DONT_VERIFY)?;
        if !under(&self.root, &now) {
            bail!("{} is not on the NAS", k.name);
        }
        // Refused, or (before the worker got to it in this run) never adopted.
        let stranded = self.rel(&now).filter(|r| !r.is_empty() && (self.refused.lock().unwrap().contains(&r.to_lowercase()) || inspect(&now, 0).is_ok_and(|l| !l.placeholder)));
        let default = restore(k)?;
        let mut report = MoveReport { from: clean(&now), to: clean(&default), refused: stranded.is_some(), ..Default::default() };
        if let Some(rel) = stranded {
            // Marked as ours: leaving the sync folder must not delete anything on the NAS.
            self.as_ours(&[&rel], || self.move_items(&now, &default, true, false, &mut report));
            self.leave(&now, &rel);
        }
        Ok(report)
    }
}

/// Returns the default place, where the folder now is.
fn restore(k: &Known) -> Result<PathBuf> {
    let default = known_path(&k.id, KF_FLAG_DEFAULT_PATH | KF_FLAG_DONT_VERIFY)?;
    fs::create_dir_all(&default).with_context(|| format!("cannot create {}", default.display()))?;
    set_known_path(&k.id, &default)?;
    log::info!("{} is back in {}", k.name, default.display());
    Ok(default)
}

/// Before the sync folder stops being one: every known folder inside it goes
/// back to its default place, so Windows is never left pointing at shells.
pub(super) fn restore_all(root: &Path) {
    for k in &KNOWN {
        match known_path(&k.id, KF_FLAG_DONT_VERIFY) {
            Ok(p) if under(root, &p) => {
                if let Err(e) = restore(k) {
                    log::error!("could not move {} back: {e:#}", k.name);
                } else {
                    println!("{} is back in its usual place; its files are still in {}", k.name, p.display());
                }
            }
            _ => {}
        }
    }
}

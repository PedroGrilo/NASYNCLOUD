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
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use nasyncloud::remote;
use serde::Serialize;
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    SHChangeNotify, SHGetKnownFolderPath, SHSetKnownFolderPath, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Pictures, KF_FLAG_DEFAULT_PATH, KF_FLAG_DONT_VERIFY, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
};

use super::{clean, unix_now, Engine};

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
    /// Where it goes (or is) in the sync folder.
    pub nas_path: String,
}

/// What moving a folder did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MoveReport {
    pub moved: usize,
    /// Items that could not be moved (they stay in `from`), with the reason.
    pub left: Vec<String>,
    pub from: String,
    pub to: String,
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
    pub(super) fn folders(&self) -> Result<Vec<FolderView>> {
        KNOWN
            .iter()
            .map(|k| {
                let path = known_path(&k.id, KF_FLAG_DONT_VERIFY)?;
                Ok(FolderView { key: k.key.into(), name: k.name.into(), on_nas: under(&self.root, &path), path: clean(&path), nas_path: clean(&self.root.join(k.name)) })
            })
            .collect()
    }

    /// Moves what is in the known folder into the sync folder and points
    /// Windows at it there.
    pub(super) fn move_folder(&self, key: &str) -> Result<MoveReport> {
        let k = known(key)?;
        let from = known_path(&k.id, KF_FLAG_DONT_VERIFY)?;
        let to = self.root.join(k.name);
        let mut report = MoveReport { from: clean(&from), to: clean(&to), ..Default::default() };
        if under(&self.root, &from) {
            return Ok(report); // already there
        }
        // Windows must know what the NAS has in there before anything goes in,
        // or a name could exist twice.
        self.listing(|| fs::read_dir(&self.root).map(|it| it.count()))?;
        match fs::metadata(&to) {
            Ok(m) if m.is_dir() => {
                self.listing(|| fs::read_dir(&to).map(|it| it.count()))?;
            }
            Ok(_) => bail!("{} already exists and is not a folder", to.display()),
            Err(_) => {
                fs::create_dir(&to).with_context(|| format!("cannot create {}", to.display()))?;
                self.enqueue(to.clone(), Duration::ZERO);
            }
        }
        if from.is_dir() {
            for item in fs::read_dir(&from)?.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                let src = item.path();
                // `desktop.ini` gives the old folder its translated name; links
                // like "My Music" in Documents point elsewhere: all stay.
                if matches!(name.to_ascii_lowercase().as_str(), "desktop.ini" | "thumbs.db") || fs::symlink_metadata(&src).is_ok_and(|m| m.file_type().is_symlink()) {
                    continue;
                }
                let mut dest = to.join(&name);
                if fs::symlink_metadata(&dest).is_ok() {
                    dest = to.join(remote::conflict_name(&name, unix_now(), |c| to.join(c).exists()));
                }
                match move_item(&src, &dest) {
                    Ok(()) => {
                        report.moved += 1;
                        self.enqueue(dest, Duration::ZERO);
                    }
                    Err(e) => {
                        log::warn!("could not move {} to the NAS: {e:#}", src.display());
                        report.left.push(format!("{name}: {e:#}"));
                    }
                }
            }
        }
        set_known_path(&k.id, &to)?;
        log::info!("{} is now on the NAS, in {} ({} item(s) moved, {} left in {})", k.name, to.display(), report.moved, report.left.len(), from.display());
        Ok(report)
    }

    /// Points the known folder back at its default place (empty); the files
    /// stay on the NAS. Returns where they are.
    pub(super) fn restore_folder(&self, key: &str) -> Result<String> {
        let k = known(key)?;
        let now = known_path(&k.id, KF_FLAG_DONT_VERIFY)?;
        if !under(&self.root, &now) {
            bail!("{} is not on the NAS", k.name);
        }
        restore(k)?;
        Ok(clean(&now))
    }
}

fn restore(k: &Known) -> Result<()> {
    let default = known_path(&k.id, KF_FLAG_DEFAULT_PATH | KF_FLAG_DONT_VERIFY)?;
    fs::create_dir_all(&default).with_context(|| format!("cannot create {}", default.display()))?;
    set_known_path(&k.id, &default)?;
    log::info!("{} is back in {}", k.name, default.display());
    Ok(())
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

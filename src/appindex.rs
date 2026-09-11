//! The application index: every launchable app with its icon, built out of
//! process and kept on disk.
//!
//! Enumerating the shell's AppsFolder and extracting icons drags some forty
//! DLLs into whichever process does it - `windows.storage`, the state
//! repository, the imaging stack - and they never unload. Measured in a bare
//! process: 1.7 MB private before, 5.5 MB after enumerating names alone,
//! 7.6 MB once icons are extracted too. So the launcher never does either.
//! A helper - this same exe run with [`HELPER_FLAG`] - does the work, writes
//! the result to [`path`], and exits; the launcher reads the file, which is
//! one allocation and touches no new library.
//!
//! On every start the launcher loads the previous index immediately, so the
//! first summon already has everything, and runs the helper in the
//! background to pick up installs and removals. `Apex: Reload` runs it too.
//! The file lives beside the launch history in `%LOCALAPPDATA%\apex`:
//! machine-local, rebuilt on demand, safe to delete.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{
    COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, SIID_APPLICATION,
    SIID_FOLDER,
};

use crate::icon;
use crate::plugin::Icon;

/// Command-line flag that turns the exe into the index helper.
pub const HELPER_FLAG: &str = "--index";

/// What counts as launchable in a user-added source folder.
const SOURCE_EXTENSIONS: [&str; 5] = ["exe", "lnk", "bat", "cmd", "ps1"];

/// How long the launcher waits for the helper before giving up on it.
/// Indexing takes about a second; this only matters if the shell hangs.
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

const MAGIC: &[u8; 8] = b"APEXIDX1";
/// "No icon" marker in the file.
const NO_BLOB: u32 = u32::MAX;
/// Sanity limits so a corrupt file cannot ask for absurd allocations.
const MAX_ICON_SIDE: u16 = 256;
const MAX_ENTRIES: u32 = 100_000;

/// One launchable application.
pub struct IndexedApp {
    /// Display name as shown in the Start menu, or the file stem for a
    /// source-folder entry.
    pub name: String,
    /// AppsFolder parsing name, launched as `shell:AppsFolder\<id>`; or the
    /// full path of a source-folder file.
    pub app_id: String,
    pub icon: Option<Arc<Icon>>,
}

#[derive(Default)]
pub struct AppIndex {
    pub apps: Vec<IndexedApp>,
    /// The shell's generic application icon, for entries without their own.
    pub fallback: Option<Arc<Icon>>,
    /// The shell's folder icon, for the source-folder listing.
    pub folder: Option<Arc<Icon>>,
}

pub fn path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("apex").join("index.bin"))
}

/// The index as last written by the helper, if there is one and it parses.
pub fn load() -> Option<AppIndex> {
    let bytes = std::fs::read(path()?).ok()?;
    let started = Instant::now();
    let index = decode(&bytes);
    crate::dlog!(
        "index: loaded {} apps from {} KB in {:.1?}",
        index.as_ref().map_or(0, |i| i.apps.len()),
        bytes.len() / 1024,
        started.elapsed()
    );
    index
}

/// Entry point for `apex --index`: build the index and write it.
///
/// Runs before anything else in `main` - no config, no single-instance
/// mutex, no window - so a helper never interferes with the launcher.
pub fn helper_main() {
    unsafe {
        // MTA: this process never pumps messages, and STA COM without a pump
        // can deadlock inside shell calls (icon extraction did exactly that).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
    }
    let started = Instant::now();
    let sources = crate::config::Config::load().list_values(crate::config::SOURCES);
    let index = build(&sources);
    match save(&index) {
        Ok(()) => crate::dlog!(
            "index helper: wrote {} apps in {:.1?}",
            index.apps.len(),
            started.elapsed()
        ),
        Err(e) => crate::dlog!("index helper: write failed: {e}"),
    }
}

/// Run the helper and wait for it. Returns whether it finished cleanly, in
/// which case [`load`] returns what it wrote.
pub fn run_helper() -> bool {
    use std::os::windows::process::CommandExt;
    /// CREATE_NO_WINDOW: debug builds are console apps, and a console
    /// flashing up at every start would be absurd.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let child = std::process::Command::new(&exe)
        .arg(HELPER_FLAG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            crate::dlog!("index: could not start helper {}: {e}", exe.display());
            return false;
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < HELPER_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                crate::dlog!("index: helper timed out, killing it");
                let _ = child.kill();
                return false;
            }
            Err(_) => return false,
        }
    }
}

// ---- building (helper process) ------------------------------------------

/// Enumerate and extract everything. This is the expensive, DLL-loading
/// part, and the reason the helper exists; the launcher only calls it as a
/// last resort when the helper cannot be run at all.
pub fn build(sources: &[String]) -> AppIndex {
    // Extracted once and shared by every entry that has no icon of its own,
    // so the fallback costs one bitmap rather than one per app.
    let fallback = icon::stock(SIID_APPLICATION).map(Arc::new);
    let mut apps = Vec::new();
    if let Err(e) = unsafe { enum_apps_folder(&mut apps, fallback.as_ref()) } {
        crate::dlog!("index: AppsFolder enumeration failed: {e}");
    }
    for dir in sources {
        scan_source(dir, &mut apps, fallback.as_ref());
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps.dedup_by(|a, b| a.name == b.name && a.app_id == b.app_id);
    AppIndex {
        apps,
        fallback,
        folder: icon::stock(SIID_FOLDER).map(Arc::new),
    }
}

unsafe fn enum_apps_folder(
    out: &mut Vec<IndexedApp>,
    fallback: Option<&Arc<Icon>>,
) -> windows::core::Result<()> {
    unsafe {
        let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
        let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
        loop {
            let mut batch: [Option<IShellItem>; 16] = Default::default();
            let mut fetched = 0u32;
            let _ = items.Next(&mut batch, Some(&mut fetched));
            if fetched == 0 {
                break;
            }
            for item in batch.iter().take(fetched as usize).flatten() {
                let Ok(name) = display_name(item, SIGDN_NORMALDISPLAY) else {
                    continue;
                };
                let Ok(app_id) = display_name(item, SIGDN_PARENTRELATIVEPARSING) else {
                    continue;
                };
                if name.is_empty() || app_id.is_empty() {
                    continue;
                }
                out.push(IndexedApp {
                    name,
                    app_id,
                    icon: icon::from_shell_item(item)
                        .map(Arc::new)
                        .or_else(|| fallback.cloned()),
                });
            }
        }
        Ok(())
    }
}

/// Index launchable files sitting directly in a user-added source folder.
///
/// One level only, deliberately: a source pointed at a deep tree - or at a
/// drive root by mistake - would otherwise stall indexing.
fn scan_source(dir: &str, out: &mut Vec<IndexedApp>, fallback: Option<&Arc<Icon>>) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        crate::dlog!("index: source unreadable, skipping: {dir}");
        return;
    };
    let mut found = 0usize;
    for entry in listing.flatten() {
        let path = entry.path();
        let is_launchable = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| SOURCE_EXTENSIONS.iter().any(|w| ext.eq_ignore_ascii_case(w)));
        if !is_launchable || !path.is_file() {
            continue;
        }
        let (Some(name), Some(full)) = (path.file_stem().and_then(|s| s.to_str()), path.to_str())
        else {
            continue;
        };
        out.push(IndexedApp {
            name: name.to_string(),
            app_id: full.to_string(),
            icon: icon::from_path(full)
                .map(Arc::new)
                .or_else(|| fallback.cloned()),
        });
        found += 1;
    }
    crate::dlog!("index: source {dir} contributed {found} entries");
}

unsafe fn display_name(item: &IShellItem, kind: SIGDN) -> windows::core::Result<String> {
    unsafe {
        let pw = item.GetDisplayName(kind)?;
        let s = pw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pw.0 as *const _));
        Ok(s)
    }
}

// ---- file format ----------------------------------------------------------
//
// Little-endian throughout:
//
//   "APEXIDX1"
//   u32 blob count, then per blob: u16 width, u16 height, width*height*4 BGRA
//   u32 fallback blob, u32 folder blob        (NO_BLOB when absent)
//   u32 app count, then per app: u16 len + name, u16 len + app id, u32 blob
//
// Bitmaps are stored once and referenced by index: many apps share the
// generic icon, and several Store apps share one image, so this keeps the
// file at a couple of megabytes rather than ten.

fn save(index: &AppIndex) -> std::io::Result<()> {
    let path = path().ok_or(std::io::ErrorKind::NotFound)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written beside the target and renamed into place, so a launcher
    // reading concurrently sees either the old file or the new one.
    let tmp = path.with_extension(format!("bin.{}.tmp", std::process::id()));
    std::fs::write(&tmp, encode(index))?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn encode(index: &AppIndex) -> Vec<u8> {
    let mut blobs: Vec<Arc<Icon>> = Vec::new();
    // Dedup by pointer first (shared Arcs), then by content, since the same
    // image extracted twice is two allocations with identical pixels.
    let mut by_ptr: Vec<(*const Icon, u32)> = Vec::new();
    let mut by_hash: Vec<(u64, u32)> = Vec::new();
    let mut blob_of = |icon: &Option<Arc<Icon>>| -> u32 {
        let Some(icon) = icon else { return NO_BLOB };
        let ptr = Arc::as_ptr(icon);
        if let Some((_, i)) = by_ptr.iter().find(|(p, _)| *p == ptr) {
            return *i;
        }
        let hash = fnv1a(icon);
        if let Some((_, i)) = by_hash
            .iter()
            .find(|(h, i)| *h == hash && same_pixels(&blobs[*i as usize], icon))
        {
            by_ptr.push((ptr, *i));
            return *i;
        }
        let i = blobs.len() as u32;
        blobs.push(icon.clone());
        by_ptr.push((ptr, i));
        by_hash.push((hash, i));
        i
    };

    let app_blobs: Vec<u32> = index.apps.iter().map(|a| blob_of(&a.icon)).collect();
    let fallback = blob_of(&index.fallback);
    let folder = blob_of(&index.folder);

    let mut out = Vec::with_capacity(blobs.iter().map(|b| b.bgra.len() + 4).sum::<usize>() + 4096);
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, blobs.len() as u32);
    for b in &blobs {
        put_u16(&mut out, b.width as u16);
        put_u16(&mut out, b.height as u16);
        out.extend_from_slice(&b.bgra);
    }
    put_u32(&mut out, fallback);
    put_u32(&mut out, folder);
    put_u32(&mut out, index.apps.len() as u32);
    for (app, blob) in index.apps.iter().zip(app_blobs) {
        put_str(&mut out, &app.name);
        put_str(&mut out, &app.app_id);
        put_u32(&mut out, blob);
    }
    out
}

/// Parse a file written by [`encode`]. Anything malformed - wrong magic,
/// truncated, out-of-range reference - yields `None`, never a panic: the
/// launcher then just runs the helper again.
fn decode(bytes: &[u8]) -> Option<AppIndex> {
    let mut r = Reader { bytes, pos: 0 };
    if r.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let blob_count = r.u32()?;
    if blob_count > MAX_ENTRIES {
        return None;
    }
    let mut blobs: Vec<Arc<Icon>> = Vec::with_capacity(blob_count as usize);
    for _ in 0..blob_count {
        let width = r.u16()?;
        let height = r.u16()?;
        if width == 0 || height == 0 || width > MAX_ICON_SIDE || height > MAX_ICON_SIDE {
            return None;
        }
        let bgra = r.take(width as usize * height as usize * 4)?.to_vec();
        blobs.push(Arc::new(Icon {
            width: width as u32,
            height: height as u32,
            bgra,
        }));
    }
    let blob = |i: u32| -> Option<Option<Arc<Icon>>> {
        if i == NO_BLOB {
            Some(None)
        } else {
            blobs.get(i as usize).cloned().map(Some)
        }
    };
    let fallback = blob(r.u32()?)?;
    let folder = blob(r.u32()?)?;
    let app_count = r.u32()?;
    if app_count > MAX_ENTRIES {
        return None;
    }
    let mut apps = Vec::with_capacity(app_count as usize);
    for _ in 0..app_count {
        let name = r.str()?;
        let app_id = r.str()?;
        let icon = blob(r.u32()?)?;
        apps.push(IndexedApp { name, app_id, icon });
    }
    Some(AppIndex {
        apps,
        fallback,
        folder,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(slice)
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Length-prefixed UTF-8, truncated at a char boundary if it would not fit
/// the u16 prefix - names and ids are a few dozen bytes in practice.
fn put_str(out: &mut Vec<u8>, s: &str) {
    let mut end = s.len().min(u16::MAX as usize);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    put_u16(out, end as u16);
    out.extend_from_slice(&s.as_bytes()[..end]);
}

fn fnv1a(icon: &Icon) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in icon
        .width
        .to_le_bytes()
        .iter()
        .chain(icon.height.to_le_bytes().iter())
        .chain(icon.bgra.iter())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn same_pixels(a: &Icon, b: &Icon) -> bool {
    a.width == b.width && a.height == b.height && a.bgra == b.bgra
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(fill: u8) -> Arc<Icon> {
        Arc::new(Icon {
            width: 2,
            height: 2,
            bgra: vec![fill; 16],
        })
    }

    fn app(name: &str, icon: Option<Arc<Icon>>) -> IndexedApp {
        IndexedApp {
            name: name.to_string(),
            app_id: format!("id-{name}"),
            icon,
        }
    }

    #[test]
    fn round_trips_and_shares_identical_bitmaps() {
        let shared = icon(7);
        let index = AppIndex {
            apps: vec![
                app("Alpha", Some(shared.clone())),
                app("Beta", Some(shared.clone())),
                // Same pixels, different allocation: still one blob.
                app("Gamma", Some(icon(7))),
                app("Delta", Some(icon(9))),
                app("Epsilon", None),
            ],
            fallback: Some(shared),
            folder: None,
        };
        let bytes = encode(&index);
        // Header + 2 blobs of 20 bytes + refs + 5 apps: three would be 60.
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 2);

        let back = decode(&bytes).expect("decodes");
        assert_eq!(back.apps.len(), 5);
        assert_eq!(back.apps[0].name, "Alpha");
        assert_eq!(back.apps[0].app_id, "id-Alpha");
        assert_eq!(back.apps[0].icon.as_ref().unwrap().bgra, vec![7; 16]);
        assert_eq!(back.apps[3].icon.as_ref().unwrap().bgra, vec![9; 16]);
        assert!(back.apps[4].icon.is_none());
        assert!(back.folder.is_none());
        // Shared on the way in, shared on the way out.
        assert!(Arc::ptr_eq(
            back.apps[0].icon.as_ref().unwrap(),
            back.apps[2].icon.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            back.fallback.as_ref().unwrap(),
            back.apps[1].icon.as_ref().unwrap()
        ));
    }

    #[test]
    fn empty_index_round_trips() {
        let back = decode(&encode(&AppIndex::default())).expect("decodes");
        assert!(back.apps.is_empty());
        assert!(back.fallback.is_none());
    }

    #[test]
    fn unicode_names_survive() {
        let index = AppIndex {
            apps: vec![app("Café ☕ 日本語", None)],
            ..Default::default()
        };
        let back = decode(&encode(&index)).unwrap();
        assert_eq!(back.apps[0].name, "Café ☕ 日本語");
    }

    #[test]
    fn corrupt_files_are_rejected_not_panicked() {
        let index = AppIndex {
            apps: vec![app("Alpha", Some(icon(1)))],
            fallback: Some(icon(2)),
            folder: None,
        };
        let bytes = encode(&index);
        assert!(decode(&bytes).is_some());
        // Every truncation point must fail cleanly.
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_none(), "cut at {cut} decoded");
        }
        let mut wrong_magic = bytes.clone();
        wrong_magic[7] = b'9';
        assert!(decode(&wrong_magic).is_none());
        // A blob reference past the end.
        let mut bad_ref = bytes.clone();
        let n = bad_ref.len();
        bad_ref[n - 4..].copy_from_slice(&5u32.to_le_bytes());
        assert!(decode(&bad_ref).is_none());
        // A blob claiming an absurd size must not allocate for it.
        let mut huge = bytes.clone();
        huge[12..14].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(decode(&huge).is_none());
        assert!(decode(b"").is_none());
    }

    /// Which step loads the imaging and shell DLLs - the measurement the
    /// helper process exists for. Run by hand:
    ///
    /// `cargo test -- --ignored which_step_loads_shell_dlls --nocapture`
    #[test]
    #[ignore]
    fn which_step_loads_shell_dlls() {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::System::ProcessStatus::{
            K32EnumProcessModules, K32GetModuleFileNameExW, K32GetProcessMemoryInfo,
            PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;

        fn modules() -> Vec<String> {
            unsafe {
                let proc = GetCurrentProcess();
                let mut mods = vec![HMODULE::default(); 512];
                let mut needed = 0u32;
                let _ = K32EnumProcessModules(
                    proc,
                    mods.as_mut_ptr(),
                    (mods.len() * size_of::<HMODULE>()) as u32,
                    &mut needed,
                );
                let n = (needed as usize / size_of::<HMODULE>()).min(mods.len());
                let mut out: Vec<String> = mods[..n]
                    .iter()
                    .map(|m| {
                        let mut buf = [0u16; 520];
                        let len = K32GetModuleFileNameExW(Some(proc), Some(*m), &mut buf) as usize;
                        let full = String::from_utf16_lossy(&buf[..len]);
                        full.rsplit('\\').next().unwrap_or(&full).to_lowercase()
                    })
                    .collect();
                out.sort();
                out
            }
        }
        fn private_mb() -> f64 {
            unsafe {
                let mut c = PROCESS_MEMORY_COUNTERS_EX {
                    cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    ..Default::default()
                };
                let _ = K32GetProcessMemoryInfo(
                    GetCurrentProcess(),
                    &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                    c.cb,
                );
                c.PrivateUsage as f64 / 1e6
            }
        }
        fn report(stage: &str, before: &[String]) -> Vec<String> {
            let now = modules();
            let added: Vec<&String> = now.iter().filter(|m| !before.contains(m)).collect();
            eprintln!(
                "== {stage}: {} modules, {:.1} MB private, added {:?}",
                now.len(),
                private_mb(),
                added
            );
            now
        }

        let m0 = report("start", &[]);
        let loaded = load();
        let m1 = report(
            &format!("after load() ({} apps)", loaded.map_or(0, |i| i.apps.len())),
            &m0,
        );
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
        }
        let mut apps = Vec::new();
        let _ = unsafe { enum_apps_folder(&mut apps, None) };
        let m2 = report(&format!("after enumerating {} apps", apps.len()), &m1);
        let _ = icon::stock(SIID_APPLICATION);
        report("after icon::stock", &m2);
    }
}

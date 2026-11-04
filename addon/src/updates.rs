use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};

use crate::Result;

const REPO: &str = "shrey2199/swofty_customplay_addon";
const ADDON: &str = "local.custom-play";
const MAX_PACKAGE_SIZE: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Build {
    base: Version,
    patch: u32,
}

impl Build {
    fn from_tag(tag: &str) -> Option<Self> {
        let version = tag.strip_prefix('v')?;
        if let Some((base, patch)) = version.split_once("_patch") {
            let patch = patch
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0 && *n <= i32::MAX as u32)?;
            let build = Self::from_manifest(&format!("{base}+patch.{patch}"))?;
            (build.tag() == tag).then_some(build)
        } else {
            let build = Self::from_manifest(version)?;
            (build.patch == 0).then_some(build)
        }
    }

    fn from_manifest(version: &str) -> Option<Self> {
        let mut base = Version::parse(version).ok()?;
        if !base.pre.is_empty() {
            return None;
        }
        let patch = if base.build.is_empty() {
            0
        } else {
            base.build
                .as_str()
                .strip_prefix("patch.")?
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)?
        };
        base.build = semver::BuildMetadata::EMPTY;
        Some(Self { base, patch })
    }

    fn tag(&self) -> String {
        if self.patch == 0 {
            format!("v{}", self.base)
        } else {
            format!("v{}_patch{}", self.base, self.patch)
        }
    }

    fn asset_name(&self) -> String {
        format!("{ADDON}-{}-windows-x64.swoftyaddon", &self.tag()[1..])
    }
}

fn installed_build(root: &Path) -> Option<Build> {
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("swofty.json")).ok()?).ok()?;
    Build::from_manifest(manifest["version"].as_str()?)
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

impl Asset {
    fn hash(&self) -> Option<&str> {
        let hex = self.digest.as_deref()?.strip_prefix("sha256:")?;
        (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
    }

    fn valid_for(&self, build: &Build) -> bool {
        self.name == build.asset_name()
            && (1..=MAX_PACKAGE_SIZE).contains(&self.size)
            && self.hash().is_some()
    }
}

#[derive(Debug)]
struct Update {
    build: Build,
    notes: String,
    asset: Asset,
}

fn newest(releases: Vec<Release>, installed: &Build, launcher: &Version) -> Option<Update> {
    releases
        .into_iter()
        .filter_map(|release| {
            if release.draft || release.prerelease {
                return None;
            }
            let build = Build::from_tag(&release.tag_name).filter(|build| build > installed)?;
            let asset = release.assets.into_iter().find(|asset| asset.valid_for(&build))?;
            let notes = release
                .body
                .unwrap_or_default()
                .trim()
                .chars()
                .take(12_000)
                .collect();
            Some(Update { build, notes, asset })
        })
        .max_by(|a, b| (a.build.base <= *launcher, &a.build).cmp(&(b.build.base <= *launcher, &b.build)))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn check(installed: Build, launcher: Version) -> Result<Option<Update>> {
    let releases = agent()
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases?per_page=100"
        ))
        .header("User-Agent", "Swofty-Custom-Play")
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_json()?;
    Ok(newest(releases, &installed, &launcher))
}

#[derive(Default)]
pub struct Updates {
    versions: Option<(Build, Version)>,
    checked: Option<Instant>,
    check_job: Option<JoinHandle<std::result::Result<Option<Update>, String>>>,
    available: Option<Update>,
    check_error: Option<String>,
    worker: Option<Child>,
    last_status: Option<Status>,
    dir: PathBuf,
}

impl Updates {
    pub fn init(&mut self, launcher: Option<&str>, data: &Path) {
        self.dir = data.join("updates");
        self.versions = (|| {
            let mut launcher = Version::parse(launcher?).ok()?;
            launcher.build = semver::BuildMetadata::EMPTY;
            let exe = std::env::current_exe().ok()?;
            Some((installed_build(exe.parent()?.parent()?)?, launcher))
        })();
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if self.check_job.as_ref().is_some_and(|job| job.is_finished()) {
            let result = self
                .check_job
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err("Update check stopped unexpectedly".into()));
            self.check_error = None;
            match result {
                Ok(update) => self.available = update,
                Err(error) => {
                    self.check_error = Some(format!("Could not check for Custom Play updates: {error}"))
                }
            }
            changed = true;
        }
        if self.check_job.is_none()
            && self
                .checked
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(15 * 60))
            && let Some((installed, launcher)) = self.versions.clone()
        {
            self.checked = Some(Instant::now());
            self.check_job = Some(thread::spawn(move || {
                check(installed, launcher).map_err(|e| e.to_string())
            }));
        }
        if self
            .worker
            .as_mut()
            .is_some_and(|child| !matches!(child.try_wait(), Ok(None)))
        {
            self.worker = None;
            changed = true;
        }
        let status = read_status(&self.dir.join("status.json"), SystemTime::now());
        if self.last_status != status {
            self.last_status = status;
            changed = true;
        }
        changed
    }

    pub fn view(
        &self,
        session_running: bool,
        actions: &mut Vec<Value>,
        notices: &mut Vec<Value>,
        blocks: &mut Vec<Value>,
    ) {
        let Some((installed, launcher)) = &self.versions else {
            return;
        };
        if installed.base < *launcher {
            notices.push(json!({"tone":"warn", "text":format!(
                "Custom Play was built for Swofty {}. You're running {}.", installed.base, launcher
            )}));
        }
        if let Some(error) = &self.check_error {
            notices.push(json!({"tone":"muted", "text":error}));
        }
        if let Some(status) = &self.last_status {
            let interrupted = status.working && !self.working();
            notices.push(json!({
                "tone": if interrupted { "warn" } else { &status.tone },
                "text": if interrupted { "The previous update was interrupted. You can try again." } else { &status.text }
            }));
            if status.working
                && !interrupted
                && let Some(percent) = status.percent
            {
                blocks.push(json!({"meters":[{
                    "label":"Custom Play update", "value":percent, "max":100,
                    "text":format!("{percent}%"), "tone":"accent"
                }]}));
            }
        }
        if let Some(update) = &self.available {
            let compatible = update.build.base <= *launcher;
            let tag = update.build.tag();
            let text = if compatible {
                format!("Custom Play {tag} is available.")
            } else {
                format!(
                    "Custom Play {tag} is available and requires Swofty {} or newer.",
                    update.build.base
                )
            };
            notices.push(json!({"tone":"accent", "text":text}));
            blocks.push(json!({"label":"What's New"}));
            let notes = if update.notes.is_empty() {
                "No release notes were provided."
            } else {
                &update.notes
            };
            blocks.push(json!({"text":{"text":notes,"tone":"neutral"}}));
            if compatible && !session_running && !self.working() {
                actions.push(json!({"id":"update", "label":"Update Custom Play"}));
            }
        }
    }

    fn working(&self) -> bool {
        self.worker.is_some()
            || open_lock(&self.dir.join("worker.lock"), false)
                .is_ok_and(|file| matches!(file.try_lock(), Err(TryLockError::WouldBlock)))
    }

    pub fn start(&mut self, data: &Path) -> Result<()> {
        let (_, launcher) = self.versions.as_ref().ok_or("No installed Custom Play version")?;
        let update = self
            .available
            .as_ref()
            .filter(|u| u.build.base <= *launcher)
            .ok_or("No compatible update available")?;
        fs::create_dir_all(&self.dir)?;
        let start_lock = open_lock(&self.dir.join("start.lock"), true)?;
        start_lock.try_lock()?;
        if self.working() {
            return Err("A Custom Play update is already in progress".into());
        }
        let pid = parent_pid()?;
        let (_, launcher_exe) = launcher_process(pid)?;
        let worker = self.dir.join("update-worker.exe");
        fs::copy(std::env::current_exe()?, &worker)?;
        let request = Request {
            pid,
            helper_pid: std::process::id(),
            launcher: launcher_exe,
            data: data.to_owned(),
            tag: update.build.tag(),
            asset: update.asset.clone(),
        };
        let request_path = self.dir.join("request.json");
        fs::write(&request_path, serde_json::to_vec(&request)?)?;
        let log = File::create(self.dir.join("update.log"))?;
        self.worker = Some(
            Command::new(worker)
                .arg("--apply-update")
                .arg(request_path)
                // Swofty starts helpers inside the installed addon. Inheriting that directory
                // keeps it in use on Windows even after the launcher and helper have exited.
                .current_dir(&self.dir)
                .creation_flags(0x08000000)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()?,
        );
        Ok(())
    }
}

#[derive(Deserialize, PartialEq, Serialize)]
struct Status {
    tone: String,
    text: String,
    working: bool,
    #[serde(default)]
    percent: Option<u8>,
}

fn read_status(path: &Path, now: SystemTime) -> Option<Status> {
    let file = File::open(path).ok()?;
    let status: Status = serde_json::from_reader(&file).ok()?;
    if status.tone == "good" && !status.working {
        let modified = file.metadata().ok()?.modified().ok()?;
        if now.duration_since(modified).ok()? >= Duration::from_secs(15) {
            return None;
        }
    }
    Some(status)
}

fn status(dir: &Path, tone: &str, text: &str, working: bool) -> Result<()> {
    write_status(dir, tone, text, working, None)
}

fn progress(dir: &Path, text: &str, percent: u8) -> Result<()> {
    write_status(dir, "accent", text, true, Some(percent))
}

fn write_status(dir: &Path, tone: &str, text: &str, working: bool, percent: Option<u8>) -> Result<()> {
    let temp = dir.join("status.tmp");
    fs::write(
        &temp,
        serde_json::to_vec(&Status {
            tone: tone.into(),
            text: text.into(),
            working,
            percent,
        })?,
    )?;
    fs::rename(temp, dir.join("status.json"))?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct Request {
    pid: u32,
    helper_pid: u32,
    launcher: PathBuf,
    data: PathBuf,
    tag: String,
    asset: Asset,
}

fn parent_pid() -> io::Result<u32> {
    // SAFETY: the snapshot and entry are valid until the enumeration ends.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let _snapshot = OwnedHandle::from_raw_handle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..std::mem::zeroed()
        };
        let mut found = Process32FirstW(snapshot, &mut entry);
        while found != 0 {
            if entry.th32ProcessID == std::process::id() {
                return Ok(entry.th32ParentProcessID);
            }
            found = Process32NextW(snapshot, &mut entry);
        }
    }
    Err(io::Error::other("Could not identify the Swofty Launcher process"))
}

fn launcher_process(pid: u32) -> Result<(OwnedHandle, PathBuf)> {
    use std::os::windows::ffi::OsStringExt;
    let mut path = vec![0u16; 32_768];
    let mut len = path.len() as u32;
    // SAFETY: the process handle is owned here and the buffer is writable for len code units.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let owned = OwnedHandle::from_raw_handle(handle);
        if QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(&path[..len as usize]));
        if !path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("swofty-launcher.exe"))
        {
            return Err("Could not locate the official Swofty Launcher executable".into());
        }
        Ok((owned, path))
    }
}

fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new()
        .create(create)
        .truncate(false)
        .write(true)
        .open(path)
}

fn wait_for_exit(process: &OwnedHandle, timeout_ms: u32) -> io::Result<bool> {
    // SAFETY: process owns the handle for the duration of the wait.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), timeout_ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn close_launcher(pid: u32) -> io::Result<bool> {
    struct Close {
        pid: u32,
        posted: bool,
    }
    unsafe extern "system" fn close_window(window: HWND, param: LPARAM) -> i32 {
        // SAFETY: EnumWindows invokes this callback synchronously with a pointer to Close.
        unsafe {
            let close = &mut *(param as *mut Close);
            let mut owner = 0;
            GetWindowThreadProcessId(window, &mut owner);
            if owner == close.pid && IsWindowVisible(window) != 0 {
                close.posted |= PostMessageW(window, WM_CLOSE, 0, 0) != 0;
            }
        }
        1
    }
    let mut close = Close { pid, posted: false };
    // SAFETY: close outlives the synchronous enumeration.
    if unsafe { EnumWindows(Some(close_window), (&mut close as *mut Close) as LPARAM) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(close.posted)
}

fn download(request: &Request, build: &Build, package: &Path) -> Result<()> {
    if !request.asset.valid_for(build) {
        return Err("The GitHub asset does not match this update".into());
    }
    let url = format!(
        "https://github.com/{REPO}/releases/download/{}/{}",
        build.tag(),
        request.asset.name
    );
    let mut response = agent()
        .get(url)
        .header("User-Agent", "Swofty-Custom-Play")
        .call()?;
    let mut file = File::create(package)?;
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(request.asset.size + 1)
        .reader();
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 32 * 1024];
    let mut bytes = 0;
    let mut last_percent = 0;
    let mut reported = Instant::now();
    loop {
        let len = reader.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        file.write_all(&buffer[..len])?;
        digest.update(&buffer[..len]);
        bytes += len as u64;
        let percent = (bytes * 100 / request.asset.size).min(100) as u8;
        if percent != last_percent && reported.elapsed() >= Duration::from_millis(150) {
            progress(
                package.parent().unwrap(),
                "Downloading the Custom Play update…",
                percent,
            )?;
            last_percent = percent;
            reported = Instant::now();
        }
    }
    drop(file);
    progress(
        package.parent().unwrap(),
        "Verifying the Custom Play update…",
        100,
    )?;
    let hash: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if bytes != request.asset.size || !hash.eq_ignore_ascii_case(request.asset.hash().unwrap()) {
        return Err("The downloaded addon failed its SHA-256 check".into());
    }
    let mut zip = zip::ZipArchive::new(File::open(package)?)?;
    let manifest: Value = serde_json::from_reader(zip.by_name("swofty.json")?.take(64 * 1024))?;
    if manifest["id"] != ADDON
        || manifest["version"]
            .as_str()
            .and_then(Build::from_manifest)
            .as_ref()
            != Some(build)
        || manifest["compat"]["launcher"] != format!(">={}", build.base)
    {
        return Err("The downloaded addon manifest does not match the release tag".into());
    }
    Ok(())
}

pub fn apply(request_path: &Path) -> Result<()> {
    let dir = request_path.parent().ok_or("No update folder")?;
    let worker_lock = open_lock(&dir.join("worker.lock"), true)?;
    worker_lock.try_lock()?;
    let request: Request = serde_json::from_slice(&fs::read(request_path)?)?;
    let build = Build::from_tag(&request.tag).ok_or("Invalid update tag")?;
    let package = dir.join(build.asset_name());
    let mut reopen = false;
    let result = (|| -> Result<()> {
        let (handle, launcher) = launcher_process(request.pid)?;
        if launcher != request.launcher {
            return Err("Swofty Launcher changed while preparing the update".into());
        }
        // SAFETY: request names the helper that started this worker. We only wait on its handle.
        let helper = unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, request.helper_pid);
            if handle.is_null() {
                return Err(io::Error::last_os_error().into());
            }
            OwnedHandle::from_raw_handle(handle)
        };
        let cli = launcher.with_file_name("swofty.exe");
        if !cli.is_file() {
            return Err("The official swofty.exe is missing beside the launcher".into());
        }
        progress(dir, "Downloading the Custom Play update…", 0)?;
        download(&request, &build, &package)?;
        let lock_path = request.data.join("state/session.lock");
        fs::create_dir_all(lock_path.parent().unwrap())?;
        let session = open_lock(&lock_path, true)?;
        match session.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err("Finish your game session before updating Custom Play".into());
            }
            Err(error) => return Err(error.into()),
        }
        progress(dir, "Restarting Swofty to install the Custom Play update…", 100)?;
        if !wait_for_exit(&handle, 0)? && !(close_launcher(request.pid)? && wait_for_exit(&handle, 15_000)?) {
            progress(
                dir,
                "Close Swofty to finish the update. It will reopen when finished.",
                100,
            )?;
            if !wait_for_exit(&handle, 90_000)? {
                return Err("Swofty did not close. Close its other windows and retry the update.".into());
            }
        }
        reopen = true;
        if !wait_for_exit(&helper, 15_000)? {
            return Err(
                "The previous Custom Play helper is still running. Close other Swofty windows and retry."
                    .into(),
            );
        }
        progress(dir, "Installing the Custom Play update…", 100)?;
        let current = installed_build(&request.data.join("addons").join(ADDON))
            .ok_or("Could not read the installed Custom Play version")?;
        if build <= current {
            return status(
                dir,
                "good",
                "The installed Custom Play is already up to date.",
                false,
            );
        }
        let output = Command::new(cli)
            .current_dir(dir)
            .arg("--data-dir")
            .arg(&request.data)
            .args(["addons", "install"])
            .arg(&package)
            .creation_flags(0x08000000)
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "Swofty could not install the update: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        status(
            dir,
            "good",
            &format!("Custom Play {} was installed.", build.tag()),
            false,
        )
    })();
    fs::remove_file(package).ok();
    if let Err(error) = &result {
        let _ = status(dir, "warn", &format!("Custom Play update failed: {error}"), false);
    }
    if reopen {
        Command::new(&request.launcher)
            .current_dir(request.launcher.parent().unwrap())
            .spawn()?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_status_expires_from_its_write_time() {
        let path = std::env::temp_dir().join(format!("custom-play-status-{}.json", std::process::id()));
        let mut file = File::create_new(&path).unwrap();
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        file.write_all(br#"{"tone":"good","text":"Installed","working":false}"#)
            .unwrap();
        file.set_modified(modified).unwrap();
        drop(file);
        assert!(read_status(&path, modified + Duration::from_secs(14)).is_some());
        assert!(read_status(&path, modified + Duration::from_secs(15)).is_none());

        for content in [
            br#"{"tone":"warn","text":"Failed","working":false}"#.as_slice(),
            br#"{"tone":"accent","text":"Downloading","working":true,"percent":50}"#.as_slice(),
        ] {
            let mut file = File::create(&path).unwrap();
            file.write_all(content).unwrap();
            file.set_modified(modified).unwrap();
            drop(file);
            assert!(read_status(&path, modified + Duration::from_secs(60)).is_some());
        }
        fs::remove_file(path).unwrap();
    }

    fn release(tag: &str) -> Release {
        let build = Build::from_tag(tag).unwrap();
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            body: Some(format!("Changes in {tag}")),
            assets: vec![Asset {
                name: build.asset_name(),
                size: 100,
                digest: Some(format!("sha256:{}", "0".repeat(64))),
            }],
        }
    }

    #[test]
    fn orders_addon_patches_numerically_within_the_base_version() {
        let installed = Build::from_manifest("1.8.6+patch.2").unwrap();
        let launcher = Version::parse("1.8.6").unwrap();
        let update = newest(
            vec![
                release("v1.8.6_patch10"),
                release("v1.8.6_patch3"),
                release("v1.8.7"),
            ],
            &installed,
            &launcher,
        )
        .unwrap();
        assert_eq!(update.build.tag(), "v1.8.6_patch10");
        assert_eq!(update.notes, "Changes in v1.8.6_patch10");
        assert!(Build::from_tag("v1.8.6_patch02").is_none());
        assert!(Build::from_tag("v1.8.6_patch0").is_none());
    }

    #[test]
    fn skips_equal_old_prerelease_and_unverifiable_builds() {
        let installed = Build::from_tag("v1.8.6_patch1").unwrap();
        let launcher = Version::parse("1.8.6").unwrap();
        let mut prerelease = release("v1.8.6_patch2");
        prerelease.prerelease = true;
        let mut bad_asset = release("v1.8.6_patch3");
        bad_asset.assets[0].digest = None;
        assert!(
            newest(
                vec![release("v1.8.6"), release("v1.8.6_patch1"), prerelease, bad_asset],
                &installed,
                &launcher
            )
            .is_none()
        );
    }

    #[test]
    fn warning_and_update_notes_leave_play_available() {
        let installed = Build::from_tag("v1.8.6").unwrap();
        let launcher = Version::parse("1.8.7").unwrap();
        let mut updates = Updates {
            available: newest(vec![release("v1.8.6_patch1")], &installed, &launcher),
            versions: Some((installed.clone(), launcher.clone())),
            ..Default::default()
        };
        let mut actions = vec![json!({"id":"play"})];
        let mut notices = Vec::new();
        let mut blocks = Vec::new();
        updates.view(false, &mut actions, &mut notices, &mut blocks);
        assert_eq!(actions[0]["id"], "play");
        assert_eq!(actions[1]["id"], "update");
        assert_eq!(notices[0]["tone"], "warn");
        assert_eq!(blocks[0]["label"], "What's New");
        assert_eq!(blocks[1]["text"]["text"], "Changes in v1.8.6_patch1");
        updates.available = newest(vec![release("v1.8.8")], &installed, &launcher);
        actions.truncate(1);
        updates.view(false, &mut actions, &mut notices, &mut blocks);
        assert_eq!(actions, vec![json!({"id":"play"})]);
    }
}

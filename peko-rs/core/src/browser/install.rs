//! Finding a Chromium browser, and fetching one when the computer has
//! none (ADR-119 D1).
//!
//! An installed Chrome, Edge, Brave or Chromium is used as it is. With
//! none installed, the owner may have Peko download Google's "Chrome for
//! Testing" build into the runtime's own folder (`peko browser install`);
//! nothing is downloaded unasked.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Names a browser path for this process; wins over everything found.
pub const BROWSER_PATH_ENV: &str = "PEKO_BROWSER_PATH";

/// Google's list of current Chrome for Testing builds.
const VERSIONS_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";

/// The file that names the downloaded build, inside the browser folder.
const CURRENT_FILE: &str = "current.json";

/// What a browser folder holds once a download finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Downloaded {
    pub version: String,
    /// The executable, relative to the browser folder.
    pub executable: String,
}

/// A browser this runtime can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub executable: PathBuf,
    /// `installed`, `downloaded`, or `configured` (the environment names it).
    pub source: &'static str,
}

/// The runtime's own browser folder under `runtime_dir`.
#[must_use]
pub fn browser_dir(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("browser")
}

/// The browser to start: the configured one, else an installed one, else
/// the one downloaded into `browser_dir`.
#[must_use]
pub fn find(browser_dir: &Path) -> Option<Found> {
    if let Some(path) = std::env::var_os(BROWSER_PATH_ENV).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(Found {
            executable: path,
            source: "configured",
        });
    }
    if let Some(executable) = installed() {
        return Some(Found {
            executable,
            source: "installed",
        });
    }
    downloaded(browser_dir).map(|executable| Found {
        executable,
        source: "downloaded",
    })
}

/// An installed Chromium browser, in order of preference.
#[must_use]
pub fn installed() -> Option<PathBuf> {
    installed_candidates().into_iter().find(|p| p.is_file())
}

#[cfg(target_os = "macos")]
fn installed_candidates() -> Vec<PathBuf> {
    let apps = [
        ("Google Chrome", "Google Chrome"),
        ("Microsoft Edge", "Microsoft Edge"),
        ("Brave Browser", "Brave Browser"),
        ("Chromium", "Chromium"),
    ];
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
    }
    let mut found = Vec::new();
    for (app, binary) in apps {
        for root in &roots {
            found.push(root.join(format!("{app}.app/Contents/MacOS/{binary}")));
        }
    }
    found
}

#[cfg(all(unix, not(target_os = "macos")))]
fn installed_candidates() -> Vec<PathBuf> {
    [
        "google-chrome",
        "google-chrome-stable",
        "microsoft-edge",
        "microsoft-edge-stable",
        "brave-browser",
        "chromium",
        "chromium-browser",
    ]
    .iter()
    .filter_map(|name| which::which(name).ok())
    .collect()
}

#[cfg(windows)]
fn installed_candidates() -> Vec<PathBuf> {
    let tails = [
        r"Google\Chrome\Application\chrome.exe",
        r"Microsoft\Edge\Application\msedge.exe",
        r"BraveSoftware\Brave-Browser\Application\brave.exe",
        r"Chromium\Application\chrome.exe",
    ];
    let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    let mut found = Vec::new();
    for tail in tails {
        for root in &roots {
            found.push(root.join(tail));
        }
    }
    found
}

/// The executable of the build downloaded into `browser_dir`, if whole.
#[must_use]
pub fn downloaded(browser_dir: &Path) -> Option<PathBuf> {
    let current: Downloaded =
        serde_json::from_slice(&std::fs::read(browser_dir.join(CURRENT_FILE)).ok()?).ok()?;
    let executable = browser_dir.join(current.executable);
    executable.is_file().then_some(executable)
}

/// The Chrome for Testing platform name of this computer.
fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("linux", "x86_64") => Ok("linux64"),
        ("windows", "x86_64") => Ok("win64"),
        ("windows", "x86") => Ok("win32"),
        (os, arch) => bail!(
            "Google publishes no Chrome for Testing build for {os} on {arch}; install Chrome or \
             Chromium instead"
        ),
    }
}

/// Where the executable sits inside the unpacked archive.
fn executable_in_archive(platform: &str) -> String {
    match platform {
        "mac-arm64" | "mac-x64" => format!(
            "chrome-{platform}/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
        ),
        "linux64" => "chrome-linux64/chrome".to_string(),
        other => format!("chrome-{other}/chrome.exe"),
    }
}

/// Pick this platform's stable build out of Google's list: `(version, url)`.
fn pick_download(list: &serde_json::Value, platform: &str) -> Result<(String, String)> {
    let stable = &list["channels"]["Stable"];
    let version = stable["version"]
        .as_str()
        .ok_or_else(|| anyhow!("the Chrome for Testing list names no stable version"))?;
    let url = stable["downloads"]["chrome"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["platform"] == platform)
        .and_then(|d| d["url"].as_str())
        .ok_or_else(|| anyhow!("the Chrome for Testing list has no {platform} build"))?;
    anyhow::ensure!(
        url.starts_with("https://storage.googleapis.com/")
            || url.starts_with("https://edgedl.me.gvt1.com/"),
        "the Chrome for Testing list names a download outside Google's hosts: {url}"
    );
    Ok((version.to_string(), url.to_string()))
}

/// Download Chrome for Testing into `browser_dir` and return its
/// executable. `progress` hears `(bytes so far, total if known)`.
pub async fn download(
    browser_dir: &Path,
    mut progress: impl FnMut(u64, Option<u64>) + Send,
) -> Result<(Downloaded, PathBuf)> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let platform = platform()?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("Peko/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let list: serde_json::Value = client
        .get(VERSIONS_URL)
        .send()
        .await
        .context("read Google's Chrome for Testing list")?
        .error_for_status()?
        .json()
        .await?;
    let (version, url) = pick_download(&list, platform)?;

    tokio::fs::create_dir_all(browser_dir)
        .await
        .with_context(|| format!("create {}", browser_dir.display()))?;
    let archive = browser_dir.join(format!("chrome-{version}.zip.part"));
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("download {url}"))?
        .error_for_status()?;
    let total = response.content_length();
    let mut file = tokio::fs::File::create(&archive).await?;
    let mut stream = response.bytes_stream();
    let mut done = 0u64;
    while let Some(piece) = stream.next().await {
        let piece = piece.context("download interrupted")?;
        file.write_all(&piece).await?;
        done += piece.len() as u64;
        progress(done, total);
    }
    file.flush().await?;
    drop(file);

    let target = browser_dir.join(&version);
    let unpack_from = archive.clone();
    let unpack_to = target.clone();
    tokio::task::spawn_blocking(move || {
        if unpack_to.exists() {
            std::fs::remove_dir_all(&unpack_to)?;
        }
        unzip(&unpack_from, &unpack_to)
    })
    .await??;
    let _ = tokio::fs::remove_file(&archive).await;

    let downloaded = Downloaded {
        executable: format!("{version}/{}", executable_in_archive(platform)),
        version,
    };
    let executable = browser_dir.join(&downloaded.executable);
    anyhow::ensure!(
        executable.is_file(),
        "the download unpacked without its executable ({})",
        executable.display()
    );
    let current = browser_dir.join(CURRENT_FILE);
    let tmp = current.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&downloaded)?)?;
    std::fs::rename(tmp, current)?;
    // Older builds are no longer named by anything.
    if let Ok(entries) = std::fs::read_dir(browser_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path != target {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
    Ok((downloaded, executable))
}

/// Unpack `archive` into `target`, keeping file modes and symbolic links
/// (a macOS app bundle is full of both). Entries that would land outside
/// `target` are refused.
fn unzip(archive: &Path, target: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)
        .context("the download is not a zip archive")?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let Some(relative) = entry.enclosed_name() else {
            bail!("the archive names a path outside itself: {}", entry.name());
        };
        let path = target.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::io::Read;
            use std::os::unix::fs::PermissionsExt;
            let mode = entry.unix_mode().unwrap_or(0o644);
            if mode & 0o170_000 == 0o120_000 {
                let mut link = String::new();
                entry.read_to_string(&mut link)?;
                anyhow::ensure!(
                    !Path::new(&link).is_absolute(),
                    "the archive holds an absolute link: {link}"
                );
                let _ = std::fs::remove_file(&path);
                std::os::unix::fs::symlink(&link, &path)?;
                continue;
            }
            let mut file = std::fs::File::create(&path)?;
            std::io::copy(&mut entry, &mut file)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o777))?;
        }
        #[cfg(not(unix))]
        {
            let mut file = std::fs::File::create(&path)?;
            std::io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stable_build_of_this_platform_is_picked_from_googles_list() {
        let list = serde_json::json!({"channels": {"Stable": {
            "version": "150.0.1.2",
            "downloads": {"chrome": [
                {"platform": "linux64", "url": "https://storage.googleapis.com/x/linux64/chrome-linux64.zip"},
                {"platform": "mac-arm64", "url": "https://storage.googleapis.com/x/mac-arm64/chrome-mac-arm64.zip"},
            ]}
        }}});
        let (version, url) = pick_download(&list, "mac-arm64").unwrap();
        assert_eq!(version, "150.0.1.2");
        assert!(url.ends_with("chrome-mac-arm64.zip"));
        assert!(pick_download(&list, "win64").is_err());

        let elsewhere = serde_json::json!({"channels": {"Stable": {
            "version": "1", "downloads": {"chrome": [
                {"platform": "linux64", "url": "https://example.com/chrome.zip"}]}
        }}});
        assert!(pick_download(&elsewhere, "linux64")
            .unwrap_err()
            .to_string()
            .contains("outside Google's hosts"));
    }

    #[test]
    fn a_finished_download_is_found_and_a_broken_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        assert!(downloaded(dir.path()).is_none());
        let executable = "1.0/chrome-linux64/chrome";
        std::fs::write(
            dir.path().join(CURRENT_FILE),
            serde_json::to_vec(&Downloaded {
                version: "1.0".into(),
                executable: executable.into(),
            })
            .unwrap(),
        )
        .unwrap();
        // Named, but the file is not there.
        assert!(downloaded(dir.path()).is_none());
        std::fs::create_dir_all(dir.path().join("1.0/chrome-linux64")).unwrap();
        std::fs::write(dir.path().join(executable), b"").unwrap();
        assert_eq!(downloaded(dir.path()), Some(dir.path().join(executable)));
    }

    #[test]
    fn an_archive_unpacks_with_modes_and_links_and_cannot_escape() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            zip.start_file("app/bin/chrome", options).unwrap();
            zip.write_all(b"#!/bin/sh\n").unwrap();
            zip.add_symlink("app/current", "bin", options).unwrap();
            zip.finish().unwrap();
        }
        let target = dir.path().join("out");
        unzip(&archive, &target).unwrap();
        assert_eq!(
            std::fs::read(target.join("app/bin/chrome")).unwrap(),
            b"#!/bin/sh\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(target.join("app/bin/chrome"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
            assert_eq!(
                std::fs::read_link(target.join("app/current")).unwrap(),
                PathBuf::from("bin")
            );
        }
    }
}

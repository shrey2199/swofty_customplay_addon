# Custom Play builder for Swofty Launcher

Builds the Windows x64 Custom Play addon and its patched `swofty.exe` from a tagged [Swofty Launcher release](https://github.com/Swofty-Developments/SwoftyLauncher/releases). The addon uses the game folder and profile selected in the official launcher and starts the batch file you select for that folder.

## Build on Windows

Install PowerShell 7.2 or newer, Git for Windows, Rust via rustup, and the Visual Studio C++ build tools with a Windows SDK. Rustup uses the toolchain pinned by the upstream release. Internet access is needed for the default build to fetch Swofty and any uncached Rust dependencies.

From this folder, run the latest stable release build or select a release tag explicitly.

```powershell
pwsh -NoProfile -File ./Build-CustomPlay.ps1
pwsh -NoProfile -File ./Build-CustomPlay.ps1 -Tag v1.8.6
```

The script checks out the release tag, checks and applies `patches/custom-play-cli.patch`, verifies the official Windows ZIP against the release's `SHA256SUMS`, then builds the CLI and the addon helper with their lockfiles. It runs the Custom Play session tests with a stand-in game process. Both packages use the native loader and DXC from the same release.

Successful builds go in `dist/<tag>/`.

- `local.custom-play-<version>-windows-x64.swoftyaddon` is the installable addon. Its version follows the upstream tag so successive builds can update the same addon ID.
- `custom-play-runner-<tag>-windows-x64.zip` contains `bin/swofty.exe`, `loader/`, and `dxc/` for standalone use. Keep those folders together when extracting it.
- `SHA256SUMS` covers both downloads. `BUILD_INFO.json` inside each package records the upstream commit and the hashes of the patch and original Windows ZIP.

Drag the `.swoftyaddon` into the matching official launcher, enable **Custom Play** for your profile, and open **Custom Play** in the sidebar. Choose your `.bat` and click **Play Custom**. The batch runs from the selected game folder. **Open launch log** shows the session output.

## Publish and install addon updates

Use `-AddonPatch` when changing Custom Play on the same upstream release.

```powershell
pwsh -NoProfile -File ./Build-CustomPlay.ps1 -Tag v1.8.6 -AddonPatch 1
```

Publish `dist/v1.8.6_patch1/local.custom-play-1.8.6_patch1-windows-x64.swoftyaddon` on the `v1.8.6_patch1` tag in [the Custom Play repository](https://github.com/shrey2199/swofty_customplay_addon/releases). Keep the generated asset name and enter the changes in the GitHub release description. Use `v1.8.6_patch2` for the next addon-only update, or the plain upstream version tag when moving to a newer Swofty release. The addon manifest uses a valid SemVer version such as `1.8.6+patch.1`.

Install the first updater-enabled build manually. The sidebar then checks GitHub in the background and shows the available release's notes under **What's New**. **Update Custom Play** downloads and verifies the package. Once prompted, close Swofty and the updater will install the package and reopen the launcher. A newer launcher version or an available update leaves **Play Custom** enabled. The update log is saved as `updates/update.log` in Custom Play's addon data folder.

## Check or rebuild a release

```powershell
pwsh -NoProfile -File ./Build-CustomPlay.ps1 -Tag v1.8.6 -CheckPatch
```

`-CheckPatch` needs Git and PowerShell only. If an upstream change conflicts with the patch, the build stops before compiling or packaging. Update the patch for that tag and run a full build to validate it. An existing output folder for a tag is preserved. Use `-OutputDirectory` to build another copy.

To use a local upstream Git repository and a previously downloaded official ZIP, pass both paths and an explicit tag. The local repository must contain the tag. Check the local ZIP against the upstream checksum before supplying it. `-RustToolchain stable` can override the upstream Rust pin when you have a compatible toolchain installed.

```powershell
pwsh -NoProfile -File ./Build-CustomPlay.ps1 -Tag v1.8.6 `
  -SourceRepository 'C:\path\to\SwoftyLauncher' `
  -ReleaseZip 'C:\downloads\swofty-launcher-v1.8.6-windows-x86_64.zip'
```

## Repository contents

- `Build-CustomPlay.ps1` is the manual build entry point.
- `patches/custom-play-cli.patch` contains the changes to the CLI and shared launch configuration, including the Windows batch session tests.
- `addon/` contains the helper source, its Cargo lockfile, and the addon manifest template.

This folder is the repository root. Git ignores downloaded sources, build intermediates, and generated packages. The generated packages include upstream third-party notices.

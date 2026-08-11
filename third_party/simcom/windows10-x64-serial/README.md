# SIMCom Windows 10 x64 serial driver

This is the local staging directory for the minimal SIMCom filter and serial
driver packages required by the A7670C-LANS on Windows 10 and Windows 11 x64.
The repository intentionally does **not** contain or distribute the vendor INF,
CAT, or SYS files. Those extensions are ignored by `.gitignore`.

Builders must obtain `Windows10.zip` through an authorized source, such as the
[SIMCom A7670 product/support page](https://www.simcom.com/product/A7670X.html),
SIMCom technical support, the modem distributor, or the organization's approved
internal software source. Do not substitute files from an untrusted driver
mirror. The source archive must not be committed.

- Source ZIP SHA-256: `F15D32F45114DE499A770C55477BF808CC9026C736CB28060BD0B906B1758024`
- Vendor files are copied byte-for-byte; changing an INF, CAT, or SYS invalidates
  the recorded hashes and can invalidate the catalog signature.
- The x86, modem, WWAN, GNSS, QDSS, and ADB packages are intentionally omitted.

`manifest.json` records the approved individual-file hashes without including
the vendor files. Run `scripts\validate-simcom-driver.ps1` from the repository
root to verify the locally staged layout, hashes, Microsoft catalog signatures,
and A7670 USB hardware-ID coverage. The Tauri bundle preparation script runs the
same validation automatically and fails if the local payload is absent.

## Prepare the bundle payload from the vendor ZIP

Perform these steps on every new build checkout because the six vendor files are
not distributed with the repository. Keep the downloaded archive outside the
repository.

Run the preparation script in PowerShell from the repository root, passing the
downloaded archive's location:

```powershell
.\scripts\prepare-simcom-driver.ps1 -SourceZip 'D:\path\to\Windows10.zip'
```

The script refuses an archive whose SHA-256 does not match `manifest.json`,
extracts it to a unique temporary directory, copies only the six allowlisted x64
files into the local staging directory, validates their hashes, catalog
signatures, and hardware IDs, and removes the temporary extraction. It never
copies the source ZIP into the repository.

The resulting vendor payload must contain exactly this layout:

```text
simfilter.inf
simlteusbfilter.cat
filter\amd64\simlteusbfilter.sys
simser.inf
simlteusbser.cat
serial\amd64\simlteusbser.sys
```

Do not copy the ZIP, `i386` directories, co-installers, or the modem, WWAN,
GNSS, QDSS, and ADB packages into this directory. Keep the repository-provided
`README.md` and `manifest.json`; they provide the preparation instructions and
hashes used by validation. `git status` must not show the locally staged INF,
CAT, or SYS files because they are deliberately ignored.

## Build the installer

After validation succeeds, build the per-machine NSIS installer:

```powershell
Set-Location modem-app
npm.cmd install
npm.cmd run tauri build
```

The Tauri pre-build hook validates the local payload again. The six local vendor
files are then bundled under `$INSTDIR\drivers\simcom` with the required
`filter\amd64` and `serial\amd64` subdirectories. The repository, source ZIP,
and extracted temporary directory do not distribute the driver files; only the
locally generated internal installer contains the validated payload. Do not
redistribute that installer without appropriate SIMCom permission.

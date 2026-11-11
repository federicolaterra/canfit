# App settings fixtures

Each folder is a synthetic **home folder** laid out like one OS's, read by the app config readers
(`src/estimate/apps/readers/`) through `Env::rooted(os, folder)` in unit tests, and by
`tests/cli_apps.rs` with `HOME` pointed at it on macOS and Linux. Values are synthetic; layouts
and keys follow the real apps.

| Folder | OS layout | What it covers |
|---|---|---|
| `macos` | macOS | Docker Desktop 4.17 `settings.json` (`memoryMiB`), three UTM 4.7 VMs (`config.plist` v4), IntelliJ IDEA 2020.3 install (`product-info.json`, shipped and user `idea.vmoptions`), Parallels `config.pvs`, VMware Fusion `.vmx`, VirtualBox `.vbox`, a generic JSON settings file |
| `macos-new` | macOS | Docker Desktop 4.35+ `settings-store.json` (`MemoryMiB`) preferred over `settings.json`; UTM VMs in an older layout and malformed; JetBrains settings folders without an install (newest wins) |
| `linux` | Linux | Docker Desktop `settings-store.json`; IntelliJ IDEA from JetBrains Toolbox with only the shipped `idea64.vmoptions`; DBeaver `dbeaver.ini`; VMware `.vmx`; VirtualBox registry with a relative and a missing entry plus the default folder |
| `windows` | Windows | Docker Desktop with the WSL 2 backend and `.wslconfig` `memory=6GB` (CRLF); IntelliJ IDEA in `Program Files\JetBrains` with a user `idea64.exe.vmoptions`; VMware Workstation `.vmx`; VirtualBox |
| `windows-wsl-default` | Windows | WSL 2 backend with no `memory=`: WSL's default, half of RAM (estimated) |
| `windows-hyperv` | Windows | Hyper-V backend: Docker's own `memoryMiB` |
| `empty` | any | No settings at all: "not found" |
| `broken` | any | Malformed JSON, a missing `memoryMiB`, `memory=lots`, malformed VirtualBox XML, `-Xmxlots`, options without `-Xmx` |

# Packaging

Every install method is produced from this repository and its GitHub Releases; nothing is
published to another repository or catalogue (see
[Development → Releases](../docs/open-source/development.md#releases)):

| Method | Produced by | Published to |
|---|---|---|
| Shell and PowerShell installers, archives, MSI, `canfit-update` | `release.yml` (cargo-dist) | the GitHub Release |
| `.deb`, `.rpm` | `linux-packages.yml` | the GitHub Release |
| Homebrew formula | `packaging/render.sh` (`package-manifests.yml` after a release) | `Formula/canfit.rb` in this repository |
| Scoop manifest | `packaging/render.sh` (`package-manifests.yml` after a release) | `packaging/scoop/canfit.json` in this repository |
| `cargo install --git … --tag vX.Y.Z` | the release tag | this repository |

Users install them with:

```bash
brew tap federicolaterra/canfit https://github.com/federicolaterra/canfit
brew install federicolaterra/canfit/canfit
```

```powershell
scoop install https://raw.githubusercontent.com/federicolaterra/canfit/master/packaging/scoop/canfit.json
```

The repository is not named `homebrew-…`, so `brew tap` needs its URL. Homebrew reads only the
`Formula/` folder. Scoop needs no bucket for a manifest URL and re-reads that URL on
`scoop update`.

## One source of truth

| File | Role |
|---|---|
| `release.env` | The release the manifests install: `VERSION` and the SHA-256 of the five archives. All-zero checksums mean "not released yet". |
| `templates/canfit.rb` | Homebrew formula template (macOS arm64/x86_64, Linux arm64/x86_64 musl archives) |
| `templates/canfit.json` | Scoop manifest template (Windows x86_64 zip), with `checkver` and `autoupdate` so it can move to a bucket unchanged |
| `../Formula/canfit.rb`, `scoop/canfit.json` | Rendered output. Never edit them by hand. |

```bash
packaging/render.sh                          # re-render after editing a template
packaging/render.sh --check                  # fail if the rendered files are stale (CI runs this)
packaging/render.sh 1.0.0 target/sums        # record a release from its .sha256 files, then render
```

`tests/packaging.rs` repeats the rendering on every OS and also fails if the files don't match. The
script refuses to write anything if a checksum is missing or malformed or a placeholder is left
over.

## After each release

`.github/workflows/package-manifests.yml` runs once `release.yml` succeeds for a stable tag. It
downloads the five archives and their `.sha256` files and verifies them. It then runs
`render.sh VERSION DIR` and opens a pull request against `master`, titled "chore: point Homebrew and
Scoop manifests at canfit X.Y.Z". `master` only accepts pull requests, so the workflow never pushes
to it. Until the pull request is merged, Homebrew and Scoop keep installing the previous version.

Pull requests opened with `GITHUB_TOKEN` don't start CI. Close and reopen the pull request (or run
`gh workflow run ci.yml --ref packaging/canfit-X.Y.Z`), wait for CI, then squash-merge it. The
workflow needs the repository setting *Allow GitHub Actions to create and approve pull requests*.

If the workflow can't run, do the same by hand:

```bash
gh release download v1.0.0 --repo federicolaterra/canfit --dir target/sums \
  --pattern '*.tar.xz.sha256' --pattern '*.zip.sha256'
packaging/render.sh 1.0.0 target/sums
git switch -c packaging/canfit-1.0.0
git add packaging/release.env Formula/canfit.rb packaging/scoop/canfit.json
git commit -m "chore: point Homebrew and Scoop manifests at canfit 1.0.0"
gh pr create --base master --fill
```

The checksums are the SHA-256 of the files attached to the GitHub Release. The rendered manifests
stay valid as long as those files don't change.

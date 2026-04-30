# Packaging

Scaffolding for distribution channels we own. Community-maintained channels
(Debian apt, Fedora dnf, Arch pacman, MacPorts, Chocolatey, FreeBSD pkg, Nix,
etc.) are out of scope until those communities choose to package fast-grep.

## Channels we publish to

| Channel        | Source                              | Update step (post-release)                                                                          |
| -------------- | ----------------------------------- | --------------------------------------------------------------------------------------------------- |
| GitHub Release | `.github/workflows/release.yml`     | Automatic on tag push                                                                               |
| crates.io      | `.github/workflows/release.yml`     | Automatic on tag push (requires `CRATES_IO_TOKEN` repo secret)                                      |
| Homebrew tap   | `packaging/homebrew/fast-grep.rb`   | Run `packaging/homebrew/update-formula.sh vX.Y.Z` and commit to `gmilano/homebrew-fast-grep`        |
| Scoop bucket   | `packaging/scoop/fast-grep.json`    | Run `packaging/scoop/update-manifest.sh vX.Y.Z` and commit to `gmilano/scoop-fast-grep`             |
| `.deb`         | `.github/workflows/release.yml`     | Automatic on tag push (attached to the GitHub Release; not pushed to apt)                           |

## One-time setup

### Homebrew tap

```
gh repo create gmilano/homebrew-fast-grep --public
git clone https://github.com/gmilano/homebrew-fast-grep
mkdir -p homebrew-fast-grep/Formula
cp packaging/homebrew/fast-grep.rb homebrew-fast-grep/Formula/
# placeholder SHA256s — overwritten by update-formula.sh after first release
cd homebrew-fast-grep && git add . && git commit -m "init" && git push
```

After each release:
```
OUTPUT=../homebrew-fast-grep/Formula/fast-grep.rb \
  packaging/homebrew/update-formula.sh v0.1.0
(cd ../homebrew-fast-grep && git commit -am "fast-grep 0.1.0" && git push)
```

End users:
```
brew install gmilano/fast-grep/fast-grep
```

### Scoop bucket

```
gh repo create gmilano/scoop-fast-grep --public
git clone https://github.com/gmilano/scoop-fast-grep
cp packaging/scoop/fast-grep.json scoop-fast-grep/
cd scoop-fast-grep && git add . && git commit -m "init" && git push
```

After each release:
```
OUTPUT=../scoop-fast-grep/fast-grep.json \
  packaging/scoop/update-manifest.sh v0.1.0
(cd ../scoop-fast-grep && git commit -am "fast-grep 0.1.0" && git push)
```

End users:
```
scoop bucket add fast-grep https://github.com/gmilano/scoop-fast-grep
scoop install fast-grep
```

### crates.io

1. Create an API token at https://crates.io/me
2. Add it as `CRATES_IO_TOKEN` in repo Settings -> Secrets -> Actions
3. First publish must be done manually (the bot needs to verify the crate name):
   ```
   cargo publish --dry-run
   cargo publish
   ```
   Subsequent releases are automated by the release workflow.

End users:
```
cargo install fast-grep
# or, prebuilt:
cargo binstall fast-grep
```

## Cutting a release

See [`../RELEASING.md`](../RELEASING.md) for the full procedure (version
bump, tagging, post-tag tap/bucket updates, and common failure modes).

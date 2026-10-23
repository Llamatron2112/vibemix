# openSUSE Tumbleweed package (OBS)

OBS builds offline, so the package takes two tarballs next to the spec:

- `vibemix-<version>.tar.gz`: the source tree with the VST3 SDK and CLAP
  submodules, from `make-source-tarball.sh <tag>`.
- `vendor.tar.zst`: the cargo crates in `vendor/` plus `.cargo/config.toml`,
  from `make-vendor-tarball.sh <version>`.

The spec unpacks both with `%autosetup -a1`. The binary is tied to Tumbleweed's
Qt minor; OBS rebuilds the package when Qt changes, so it stays in sync.

The 1.0.2 tag predates the move of the submodules to `third_party/`, so its
tarball keeps the `vendor/` submodule layout. Later releases use `third_party/`.

## One-time setup

```bash
sudo zypper install osc
osc meta prj home:USERNAME -e        # add <repository name="openSUSE_Tumbleweed"/> etc.
osc checkout home:USERNAME/vibemix
```

## Each release

1. Bump the version in `Cargo.toml` and `Version:` in `vibemix.spec`, and add a
   `vibemix.changes` entry.
2. Tag the release (`vX.Y.Z`) and push it. The release workflow attaches
   `vibemix-X.Y.Z.tar.gz` to the GitHub Release.
3. Build the tarballs from the tag:
   ```bash
   packaging/opensuse/make-source-tarball.sh vX.Y.Z
   packaging/opensuse/make-vendor-tarball.sh X.Y.Z
   # -> dist/vibemix-X.Y.Z.tar.gz, dist/vendor.tar.zst
   ```
4. Test a local build in the OBS checkout (`osc build` needs `sudo`, so run it in
   an interactive terminal):
   ```bash
   cp dist/vibemix-X.Y.Z.tar.gz dist/vendor.tar.zst vibemix.spec vibemix.changes ~/obs/home:USERNAME/vibemix/
   osc build openSUSE_Tumbleweed x86_64
   ```
5. Commit the package:
   ```bash
   osc add vibemix-X.Y.Z.tar.gz vendor.tar.zst vibemix.spec vibemix.changes
   osc commit -m "Update to X.Y.Z"
   ```
6. OBS builds for Tumbleweed. It schedules a rebuild automatically when Qt or
   the Rust toolchain changes.

## Notes

- `vendor.tar.zst` must be rebuilt whenever `Cargo.lock` changes.
- The build needs `zstd` (`BuildRequires`), and `make-vendor-tarball.sh` needs it
  locally.
- The runtime dependencies list the Qt 6.12 floor. OBS rebuilds against newer Qt
  minors, so the floor does not need bumping on every Tumbleweed update.

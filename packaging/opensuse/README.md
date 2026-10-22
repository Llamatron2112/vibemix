# openSUSE Tumbleweed package (OBS)

This directory holds the OBS package for Tumbleweed. OBS builds offline, so the
package is built from:

- `vibemix-<version>.tar.gz` — the source tree including the VST3 SDK and CLAP
  submodules under `third_party/` (no cargo crates);
- `vendor.tar.zst` — the cargo crates in `vendor/` plus `.cargo/config.toml`,
  produced by `make-vendor-tarball.sh` (or by the `cargo_vendor` source service).

The spec uses `%autosetup -a1`, which unpacks the vendor tarball next to the
source. The binary is tied to Tumbleweed's Qt minor; OBS rebuilds the package
when Qt changes, so it stays in sync.

## One-time setup

Install the local tools (Tumbleweed):

```bash
sudo zypper install osc obs-service-cargo
```

Create a project/package (or use an existing one), e.g.:

```bash
osc meta prj home:USERNAME -e        # add <repository name="openSUSE_Tumbleweed"/> etc.
osc checkout home:USERNAME/vibemix
```

## Each release

1. Bump the version in `Cargo.toml` and update `Version:` in `vibemix.spec`,
   `src` in `_service`, and add a `vibemix.changes` entry.
2. Generate the source tarball and the vendor tarball:
   ```bash
   packaging/opensuse/make-source-tarball.sh
   # -> dist/vibemix-<version>.tar.gz
   packaging/opensuse/make-vendor-tarball.sh
   # -> dist/vendor.tar.zst
   ```
3. Test a local build from the OBS checkout, which skips the local source
   services:
   ```bash
   cp dist/vibemix-<version>.tar.gz dist/vendor.tar.zst vibemix.spec vibemix.changes ~/obs/home:USERNAME/vibemix/
   osc build --no-service openSUSE_Tumbleweed x86_64
   ```
   `osc build` needs `sudo`, so run it in an interactive terminal.
4. Copy `vibemix.spec`, `vibemix.changes`, `_service` and the tarballs into the
   OBS checkout, then:
   ```bash
   osc add vibemix-<version>.tar.gz vibemix.spec vibemix.changes _service vendor.tar.zst
   osc commit -m "Update to <version>"
   ```
5. OBS builds for Tumbleweed. When Swift/Qt or the Rust toolchain changes, OBS
   schedules a rebuild automatically.

From the next tag, the release workflow also attaches
`vibemix-<version>.tar.gz` to the GitHub Release, so `_service` can use
`download_files` instead of uploading the tarball by hand:

```xml
<service name="download_files" mode="manual">
  <param name="url">https://github.com/Llamatron2112/vibemix/releases/download/v<version>/vibemix-<version>.tar.gz</param>
</service>
```

## Notes

- `vendor.tar.zst` is built locally with `make-vendor-tarball.sh`, and committed to
  OBS. `_service` still defines `cargo_vendor` in `manual` mode, so it is not run
  on commit; that is why the vendor archive is committed directly.
- The `zstd` build requirement is needed to unpack `vendor.tar.zst` in `%prep`.
- `_service` and the source tarball must use the same version string.
- The runtime dependencies list the Qt 6.12 floor; OBS rebuilds against newer
  Qt minors, so the floor does not need bumping on every Tumbleweed update.

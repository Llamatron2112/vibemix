# openSUSE Tumbleweed package (OBS)

This directory holds the OBS package for Tumbleweed. OBS builds offline, so the
package is built from:

- `vibemix-<version>.tar.gz` — the source tree including the vendored VST3 SDK
  and CLAP submodules (no cargo crates);
- `vendor.tar.zst` — the cargo crates plus `.cargo/config` and `Cargo.lock`,
  produced by the `cargo_vendor` source service.

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
2. Generate the source tarball:
   ```bash
   packaging/opensuse/make-source-tarball.sh
   # -> dist/vibemix-<version>.tar.gz
   ```
3. Copy `vibemix.spec`, `vibemix.changes`, `_service` and the tarball into the
   OBS checkout, then:
   ```bash
   osc add vibemix-<version>.tar.gz vibemix.spec vibemix.changes _service
   osc service manualrun          # runs cargo_vendor
   osc add vendor.tar.zst
   osc commit -m "Update to <version>"
   ```
4. OBS builds for Tumbleweed. When Swift/Qt or the Rust toolchain changes, OBS
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

- `cargo_vendor` runs **locally** (manual service); commit `vendor.tar.zst`.
- `_service` and the source tarball must use the same version string.
- The runtime dependencies list the Qt 6.12 floor; OBS rebuilds against newer
  Qt minors, so the floor does not need bumping on every Tumbleweed update.

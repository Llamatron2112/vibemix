# Fedora package (COPR)

COPR is the Fedora release channel. GitHub releases carry the Debian packages
and the two source archives, not Fedora RPMs.

`vibemix.spec` builds from the GitHub release assets:

- `vibemix-<version>.tar.gz`: source tree with the VST3 SDK and CLAP
  submodules;
- `vendor-<version>.tar.zst`: the cargo crates plus `.cargo/config.toml`.

COPR builds offline, so both are needed. The spec unpacks them with
`%autosetup -a1`. Unlike the OBS package, there is no hardcoded Qt ceiling:
COPR rebuilds when Fedora's Qt changes (enable *Auto-rebuild* on the package).

## COPR setup

Create a project with the `fedora-43-x86_64` and `fedora-44-x86_64` chroots,
then add an SCM package:

- Clone URL: `https://github.com/Llamatron2112/vibemix.git`
- Spec File: `packaging/fedora/vibemix.spec`
- Build method: `rpkg` (the default)

COPR downloads `Source0` and `Source1` while creating the SRPM, so both assets
must be on the matching GitHub release. Set the package to auto-rebuild on
dependency changes.

## Each release

1. Update `Version:` and add a `%changelog` entry in `vibemix.spec`.
2. Push the tag. The release workflow attaches `vibemix-<version>.tar.gz` and
   `vendor-<version>.tar.zst` to the GitHub release.
3. Trigger a COPR build from the web UI, or push to `main` if you set up the
   webhook.

## Local test

```bash
sudo dnf install rpm-build spectool
spectool -g -R packaging/fedora/vibemix.spec
rpmbuild -bs packaging/fedora/vibemix.spec
```

Or build in a clean chroot with `mock`:

```bash
sudo dnf install mock
sudo usermod -aG mock "$USER"      # then log in again
mock -r fedora-43-x86_64 --buildsrpm --spec packaging/fedora/vibemix.spec --sources .
mock -r fedora-43-x86_64 --rebuild ~/rpmbuild/SRPMS/vibemix-*.src.rpm
```

Launch the GUI once after installing: the QML modules are loaded at runtime, so
`--version` does not exercise them.

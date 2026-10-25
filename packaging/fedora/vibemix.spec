%global debug_package %{nil}

Name:           vibemix
Version:        1.0.2
Release:        1%{?dist}
Summary:        Modular LV2/VST3/CLAP audio mixer for JACK/PipeWire
License:        MIT OR Apache-2.0
URL:            https://github.com/Llamatron2112/vibemix
Source0:        https://github.com/Llamatron2112/vibemix/releases/download/v%{version}/vibemix-%{version}.tar.gz
Source1:        https://github.com/Llamatron2112/vibemix/releases/download/v%{version}/vendor-%{version}.tar.zst

ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc-c++
BuildRequires:  cmake
BuildRequires:  ninja-build
BuildRequires:  pkgconf-pkg-config
BuildRequires:  lld
BuildRequires:  zstd
BuildRequires:  qt6-qtbase-devel
BuildRequires:  qt6-qtdeclarative-devel
BuildRequires:  lv2-devel
BuildRequires:  lilv-devel
BuildRequires:  serd-devel
BuildRequires:  sord-devel
BuildRequires:  sratom-devel
BuildRequires:  suil-devel
BuildRequires:  pipewire-jack-audio-connection-kit-devel
BuildRequires:  libX11-devel
BuildRequires:  mesa-libGL-devel
BuildRequires:  libglvnd-devel

Requires:       qt6-qtdeclarative
Requires:       hicolor-icon-theme
Recommends:     gtk3
Recommends:     pipewire-jack-audio-connection-kit

%description
vibemix is a modular audio mixer for JACK/PipeWire with LV2, VST3 and CLAP
plugin support. It is split into a real-time audio engine and a Qt Quick
control surface that talk over a small OSC control protocol.

Features include strips and buses with pre/post-fader insert chains, per-bus
sends, external aux returns, sidechain routing, native plugin editors, an
in-app JACK patchbay, MIDI clock sync and learnable MIDI/keyboard bindings.

%prep
%autosetup -a1

%build
# Fedora ships qmake6 outside PATH.
export QMAKE=%{_libdir}/qt6/bin/qmake6
export PATH="%{_libdir}/qt6/bin:$PATH"

# Qt's Cargo link step fails with the default ld.bfd; use lld.
export RUSTFLAGS="-C link-arg=-fuse-ld=lld"

cargo build --release --locked --offline --workspace

%install
install -Dm755 target/release/vibemix-engine %{buildroot}%{_bindir}/vibemix-engine
install -Dm755 target/release/vibemix-gui %{buildroot}%{_bindir}/vibemix-gui
install -Dm755 packaging/vibemix %{buildroot}%{_bindir}/vibemix
install -Dm644 packaging/vibemix.desktop \
    %{buildroot}%{_datadir}/applications/vibemix.desktop
install -Dm644 packaging/vibemix.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/vibemix.svg

%files
%license LICENSE-MIT LICENSE-APACHE
%{_bindir}/vibemix
%{_bindir}/vibemix-engine
%{_bindir}/vibemix-gui
%{_datadir}/applications/vibemix.desktop
%{_datadir}/icons/hicolor/scalable/apps/vibemix.svg

%changelog
* Sun Oct 25 2026 Llamatron2112 <llamatron23@proton.me> - 1.0.2-1
- Initial COPR package.

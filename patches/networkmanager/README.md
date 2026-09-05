# NetworkManager external P2P group patch

This patch targets NetworkManager 1.58.1, specifically
`src/core/devices/wifi/nm-device-wifi-p2p.c` (upstream copyright 2018 Red Hat,
Inc.; SPDX-License-Identifier: LGPL-2.1-or-later). This patch is provided under
LGPL-2.1-or-later; see COPYING.LGPL. The root MIT license applies to Remote
Screen's own code, not to NetworkManager or this patch.

Upstream source: https://github.com/NetworkManager/NetworkManager/tree/1.58.1

The patch retains an externally created supplicant group interface until DOWN.
Without it, NetworkManager drops the unowned wrapper and removes the interface
created by the Rust pairing helper. Only the Wi-Fi plugin was replaced on the
tested machine; the system NetworkManager daemon was retained.

Apply from a clean upstream 1.58.1 source directory:

```sh
patch -p1 < /path/to/remote-screen/patches/networkmanager/external-p2p-group.patch
```

The tested local build used Meson/Ninja, distribution version 1.58.1-1, NSS,
and IWD enabled. From the parent of the patched `NetworkManager-1.58.1` directory:

```sh
meson setup build NetworkManager-1.58.1 --prefix=/usr --sysconfdir=/etc --localstatedir=/var --libdir=lib --libexecdir=lib --buildtype=release -Dselinux=false -Dcrypto=nss -Ddocs=false -Dman=false -Dintrospection=false -Dvapi=false -Dqt=false -Dtests=yes -Difupdown=false -Dppp=false -Dclat=false -Diwd=true -Ddist_version=1.58.1-1
ninja -C build -j 4 src/core/devices/wifi/libnm-device-plugin-wifi.so
```

PPP and CLAT are disabled only in this local build; do not install its daemon.
A plugin must be built and checked against the exact installed distribution
build. `tools/install-nm-p2p-plugin` is an installer for the original test
machine: its version, stock/candidate hashes and `.state/nm-build/build` path
are deliberately fixed. A fresh clone does not include that binary or private
build directory; a different build needs separate compatibility verification.
It is not a general-purpose installer for arbitrary Linux distributions.

The original test machine passed three selected upstream tests, an ownership
fixture and eager dynamic linking against its stock daemon. Real tests then
completed two persistent reconnects and disk restoration after restarting
NetworkManager, without new WPS; the user confirmed no repeated TV consent.
NetworkManager updates may overwrite the plugin.

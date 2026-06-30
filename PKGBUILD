# Maintainer: futpib-bot <futpib-bot@users.noreply.github.com>
pkgname=claugrep-git
pkgver=r1.0000000
pkgrel=1
pkgdesc='Browse, search, and export Claude Code conversation transcripts'
arch=('x86_64' 'aarch64')
url='https://github.com/futpib-bot/claugrep'
license=('MIT')
makedepends=('rust' 'cargo')
provides=('claugrep')
conflicts=('claugrep')
source=("$pkgname::git+https://github.com/futpib-bot/claugrep.git")
sha256sums=('SKIP')

pkgver() {
	cd "$pkgname"
	printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
	cd "$pkgname"
	export RUSTUP_TOOLCHAIN=stable
	cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
	cd "$pkgname"
	export RUSTUP_TOOLCHAIN=stable
	export CARGO_TARGET_DIR=target
	# makepkg.conf injects `-flto=auto` into CFLAGS, which makes the `cc` crate
	# compile the bundled sqlite3.c to GCC LTO bitcode. rust-lld (Rust's linker)
	# has no GCC LTO plugin and can't link that, so the final binary fails with
	# `undefined symbol: sqlite3_step`. Strip -flto for this build.
	export CFLAGS="$(echo "$CFLAGS" | sed -E 's/ ?-flto(=[^ ]*)?//g')"
	export LDFLAGS="$(echo "$LDFLAGS" | sed -E 's/ ?-flto(=[^ ]*)?//g')"
	cargo build --release --locked
}

package() {
	cd "$pkgname"
	install -Dm755 "target/release/claugrep" "$pkgdir/usr/bin/claugrep"
	install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}

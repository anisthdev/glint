# Maintainer: asifb
_name=glint
pkgname=totoro-glint
pkgver=0.1.0
pkgrel=1
pkgdesc="Small Wayland layer-shell overlay that plays event animations, like Android's charging ripple (Rust)"
arch=('x86_64')
url="https://github.com/anisthdev/glint"
provides=('glint')
conflicts=('glint')
replaces=('glint')
license=('MIT' 'Apache-2.0')
depends=('gcc-libs' 'glibc')
optdepends=('sysaud: play glint on system events such as charger connect')
makedepends=('cargo')
source=("$_name-$pkgver.tar.gz::$url/archive/v$pkgver.tar.gz")
sha256sums=('12a99a8ff8c8b4400a2dcf447ef43bfe3c367beebb71cd0a575df75302450988')

prepare() {
    cd "$_name-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$_name-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo build --frozen --release
}

check() {
    cd "$_name-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --frozen --release
}

package() {
    cd "$_name-$pkgver"
    install -Dm755 target/release/glint "$pkgdir/usr/bin/glint"
    install -Dm644 man/glint.1 "$pkgdir/usr/share/man/man1/glint.1"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$_name/README.md"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$_name/LICENSE"
    install -Dm644 LICENSE-APACHE "$pkgdir/usr/share/licenses/$_name/LICENSE-APACHE"
    install -Dm644 NOTICE "$pkgdir/usr/share/licenses/$_name/NOTICE"
}

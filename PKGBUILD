# Maintainer: asifb
pkgname=glint
pkgver=0.1.0
pkgrel=1
pkgdesc="Small Wayland layer-shell overlay that plays event animations, like Android's charging ripple (Rust)"
arch=('x86_64')
url="https://github.com/anisthdev/glint"
license=('MIT' 'Apache-2.0')
depends=('gcc-libs' 'glibc')
optdepends=('sysaud: play glint on system events such as charger connect')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/archive/v$pkgver.tar.gz")
sha256sums=('SKIP')

prepare() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo build --frozen --release
}

check() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --frozen --release
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm755 target/release/glint "$pkgdir/usr/bin/glint"
    install -Dm644 man/glint.1 "$pkgdir/usr/share/man/man1/glint.1"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
    install -Dm644 LICENSE-APACHE "$pkgdir/usr/share/licenses/$pkgname/LICENSE-APACHE"
    install -Dm644 NOTICE "$pkgdir/usr/share/licenses/$pkgname/NOTICE"
}

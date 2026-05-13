# Maintainer: Your Name <your-email@address.com>
pkgname=md-preview-git
pkgver=0.1.0
pkgrel=1
pkgdesc="A tiny Rust-based GitHub-style markdown previewer that dies after use."
arch=('x86_64')
url="https://github.com/yourusername/md-preview"
license=('MIT')
depends=('gcc-libs')
makedepends=('rust' 'cargo' 'git')
source=("git+https://github.com/yourusername/md-preview.git")
sha256sums=('SKIP')

build() {
  cd "$pkgname"
  cargo build --release --locked
}

package() {
  cd "$pkgname"
  install -Dm755 "target/release/md-preview" "$pkgdir/usr/bin/md"
}

# Build and install the binary and man pages. Supports PREFIX and DESTDIR.
#
#   make && sudo make install
#   make && make install PREFIX="$HOME/.local"
#   make install DESTDIR="$pkgdir" PREFIX=/usr

PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
MANDIR  ?= $(PREFIX)/share/man

CARGO   ?= cargo
INSTALL ?= install

BIN := target/release/wayhud

.PHONY: all build install uninstall clean

all: build

build:
	$(CARGO) build --release --locked

# Build separately so sudo install cannot leave root-owned build artifacts.
install:
	@test -x '$(BIN)' || { echo 'wayhud: $(BIN) is missing — run `make` first' >&2; exit 1; }
	$(INSTALL) -Dm755 $(BIN)            $(DESTDIR)$(BINDIR)/wayhud
	$(INSTALL) -Dm644 man/man1/wayhud.1 $(DESTDIR)$(MANDIR)/man1/wayhud.1
	$(INSTALL) -Dm644 man/man5/wayhud.5 $(DESTDIR)$(MANDIR)/man5/wayhud.5

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/wayhud
	rm -f $(DESTDIR)$(MANDIR)/man1/wayhud.1
	rm -f $(DESTDIR)$(MANDIR)/man5/wayhud.5

clean:
	$(CARGO) clean

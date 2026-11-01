# Everything runs in the Ubuntu 24.04 build container (./run-in-docker.sh) so the .deb's
# glibc/GTK (4.14) dependencies stay installable on Ubuntu 24.04+ and Debian 13+.
# With a local toolchain (rustup + libgtk-4-dev + cargo-deb), run with RUN= to skip Docker.
RUN ?= ./run-in-docker.sh

.PHONY: image build test lint deb rpm arch packages install sample realtest compare compare-rpcs3 gui clean

image:
	docker build -t pkgview-build -f Dockerfile.build .

build:
	$(RUN) cargo build --release

test:
	$(RUN) cargo test

lint:
	$(RUN) sh -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings'

# Man page and completions come from the binary itself, so build first, then package.
deb:
	$(RUN) sh -c 'cargo build --release && mkdir -p target/assets && b=target/release/pkgview && \
	  $$b man | gzip -9n > target/assets/pkgview.1.gz && $$b completions bash > target/assets/pkgview.bash && \
	  $$b completions zsh > target/assets/_pkgview && $$b completions fish > target/assets/pkgview.fish && cargo deb'
	@ls -1 target/debian/*.deb

# Same binary and assets as the .deb (run `make deb` first: it builds them).
rpm:
	$(RUN) cargo generate-rpm
	@ls -1 target/generate-rpm/*.rpm

# Arch: packaging/aur/PKGBUILD (pkgview-bin) repackages the .deb. Built and checked in an archlinux container
# with the local .deb standing in for the release download; also refreshes packaging/aur/.SRCINFO.
arch:
	tools/arch-package.sh

# All three, from one build.
packages: deb rpm arch

install: deb
	sudo apt install -y ./target/debian/$$(ls -t target/debian | head -1)

# Synthetic test package (TYPE=0x00000001 debug, 0x80000001 PS3, 0x80000002 PSP)
TYPE ?= 0x80000001
sample:
	$(RUN) env PKGVIEW_WRITE_SAMPLE=/w/sample.pkg PKGVIEW_SAMPLE_TYPE=$(TYPE) cargo test write_sample -- --ignored

# Real pkgs, mounted read-only: SHA-1, PARAM.SFO vs content ID, full extract to a temp dir (deleted after).
realtest:
	@test -d "$(SAMPLES)" || { echo "usage: make realtest SAMPLES=/folder/with/pkgs"; exit 2; }
	SAMPLES="$(abspath $(SAMPLES))" $(RUN) env PKGVIEW_SAMPLE=/samples \
	  cargo test --release real_sample -- --ignored --nocapture

# Install the test packages with RPCS3's own installer and with `pkgview install-rpcs3`, compare the trees
# (downloads and runs the latest RPCS3 AppImage)
compare-rpcs3:
	tools/compare-with-rpcs3.sh

# GUI smoke run under Xvfb: screenshots in target/gui/ (inspect them)
gui:
	$(RUN) sh -c 'cargo build && tools/gui-harness.sh'

clean:
	$(RUN) cargo clean

# Byte-compare against the original VB.NET crypto code (needs ../PkgView and the mono:6.12 image);
# SAMPLES=/folder adds real pkgs (slow: the original steps its counter block by block)
compare:
	SAMPLES="$(if $(SAMPLES),$(abspath $(SAMPLES)))" tools/compare-with-original.sh

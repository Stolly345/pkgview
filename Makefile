# Everything runs in the Debian 12 build container (./run-in-docker.sh) so the .deb's
# glibc/GTK dependencies stay installable on Debian 12 and Ubuntu 24.04+.
# With a local toolchain (rustup + libgtk-4-dev + cargo-deb), run with RUN= to skip Docker.
RUN ?= ./run-in-docker.sh

.PHONY: image build test lint deb install sample realtest compare clean

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

clean:
	$(RUN) cargo clean

# Byte-compare against the original VB.NET crypto code (needs ../PkgView and the mono:6.12 image);
# SAMPLES=/folder adds real pkgs (slow: the original steps its counter block by block)
compare:
	SAMPLES="$(if $(SAMPLES),$(abspath $(SAMPLES)))" tools/compare-with-original.sh

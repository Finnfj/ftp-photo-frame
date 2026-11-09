# Extract package version from Cargo.toml unless specified as
# environment variable.
#
# Deliberately not anchored at the end of the line: a checkout with CRLF endings leaves a
# carriage return before it, the match fails, and the `sed -i` below then wrote an empty
# version back into Cargo.toml. Only the first match counts, which is the package's own.
VERSION?=$(shell sed -n 's/^version = "\([^"]*\)".*/\1/p' ../Cargo.toml | head -1)

ifeq ($(strip $(VERSION)),)
$(error Could not determine the version from ../Cargo.toml. Pass it explicitly, e.g. make VERSION=1.2.3)
endif

arch=$(shell dpkg --print-architecture)
name=syno-photo-frame
dpkg_name=$(name)_$(VERSION)_$(arch)

all:
# Set version in Cargo.toml, note that this can leave the git worktree in a dirty state
	@sed -i 's/^version = "\(..*\)"/version = "$(VERSION)"/' ../Cargo.toml
# The web app is built in on purpose. A package that silently lacked it meant
# --web-password failing at runtime on the install path most people use, and the status
# page it serves is how a frame that has stopped says where it stopped.
	@cargo build --release --features web
# Create package build dir hierarchy
	@mkdir -p \
		$(dpkg_name)/usr/bin \
		$(dpkg_name)/usr/share/doc/$(name) \
		$(dpkg_name)/lib/systemd/system \
		$(dpkg_name)/etc/default \
		$(dpkg_name)/DEBIAN
	@chmod -R 0755 $(dpkg_name)
	@cp control $(dpkg_name)/DEBIAN/
	@cp conffiles $(dpkg_name)/DEBIAN/
	@chmod 0644 $(dpkg_name)/DEBIAN/conffiles
	@cp postinst prerm postrm $(dpkg_name)/DEBIAN/
	@chmod 0755 $(dpkg_name)/DEBIAN/postinst $(dpkg_name)/DEBIAN/prerm $(dpkg_name)/DEBIAN/postrm
# Installed but deliberately not enabled; see postinst for why
	@cp $(name).service $(dpkg_name)/lib/systemd/system/
	@chmod 0644 $(dpkg_name)/lib/systemd/system/$(name).service
	@cp $(name).default $(dpkg_name)/etc/default/$(name)
	@chmod 0644 $(dpkg_name)/etc/default/$(name)
	@cp copyright $(dpkg_name)/usr/share/doc/$(name)
	@chmod 0644 $(dpkg_name)/usr/share/doc/$(name)/copyright
# Set version and architecture in package control file
	@sed -i 's/^Version: 0.0.0/Version: $(VERSION)/' $(dpkg_name)/DEBIAN/control
	@sed -i 's/^Architecture:/Architecture: $(arch)/' $(dpkg_name)/DEBIAN/control
# Trim the binary
	@objcopy --strip-debug --strip-unneeded ../target/release/$(name) $(dpkg_name)/usr/bin/$(name)
	@chmod 0755 $(dpkg_name)/usr/bin/$(name)
	@gzip -c --best -k -n changelog > $(dpkg_name)/usr/share/doc/$(name)/changelog.gz
	@chmod 0644 $(dpkg_name)/usr/share/doc/$(name)/changelog.gz
	@dpkg-deb --root-owner-group --build $(dpkg_name)
# `lintian` needs to be installed for this to work
	@lintian $(dpkg_name).deb
# Generate SHA256 checksum file
	@sha256sum $(dpkg_name).deb > $(dpkg_name).sha256
# Clean up build directory
	@rm -r $(dpkg_name)

clean:
	@rm -rvf \
		$(name)_*_$(arch) \
		*.deb \
		*.sha256

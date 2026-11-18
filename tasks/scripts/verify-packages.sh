#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Build the Debian package and the gateway RPM inside containers, then assert
# that both payloads ship the files this repository puts in them. Unlike
# test-packaging-assets.sh, which only reads the sources, this runs the real
# builders (dpkg-deb, packit/rpmbuild) so a missing install rule or a stale
# %files list fails here.
#
# Usage:
#   tasks/scripts/verify-packages.sh [--deb] [--rpm] [--help]
#
# With no arguments both packages are built. The host only needs a container
# engine; every builder runs inside its distribution's image.
#
# Optional environment:
#   OPENSHELL_CONTAINER_ENGINE      podman or docker (default: whichever is found)
#   OPENSHELL_PREBUILT_BINARIES_DIR Directory holding openshell,
#                                   openshell-gateway, openshell-prover,
#                                   openshell-driver-vm, openshell-chutes-edge.
#                                   Default: throwaway --version stubs, which is
#                                   enough to verify packaging but not runtime.
#   OPENSHELL_DEB_VERSION           Version stamped into the deb (default: 0.0.0-verify)
#   OPENSHELL_RPM_VERSION           Version for the RPM (default: from openshell.spec)
#   OPENSHELL_RPM_RELEASE           Release for the RPM (default: 1.verify)

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

DEB_IMAGE="${OPENSHELL_DEB_IMAGE:-docker.io/library/debian:bookworm-slim}"
RPM_IMAGE="${OPENSHELL_RPM_IMAGE:-docker.io/library/fedora:44}"
DEB_VERSION="${OPENSHELL_DEB_VERSION:-0.0.0-verify}"
RPM_RELEASE="${OPENSHELL_RPM_RELEASE:-1.verify}"

# Files every package must contain. RPM lists them without the leading slash.
PACKAGE_FILES=(
	usr/bin/openshell-chutes-edge
	usr/lib/systemd/system/openshell-chutes-edge.service
	usr/lib/systemd/user/openshell-gateway.service
)

usage() {
	cat <<'EOF'
Verify the Debian and RPM packages by building them in containers.

Usage: tasks/scripts/verify-packages.sh [--deb] [--rpm] [--help]

With no arguments both packages are built and verified.
EOF
}

build_deb=false
build_rpm=false
while [ $# -gt 0 ]; do
	case "$1" in
	--deb) build_deb=true ;;
	--rpm) build_rpm=true ;;
	--help | -h)
		usage
		exit 0
		;;
	*)
		echo "error: unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
	shift
done
if [ "$build_deb" = false ] && [ "$build_rpm" = false ]; then
	build_deb=true
	build_rpm=true
fi

engine="${OPENSHELL_CONTAINER_ENGINE:-}"
if [ -z "$engine" ]; then
	for candidate in podman docker; do
		if command -v "$candidate" >/dev/null 2>&1; then
			engine="$candidate"
			break
		fi
	done
fi
if [ -z "$engine" ]; then
	echo "error: needs podman or docker; set OPENSHELL_CONTAINER_ENGINE to override" >&2
	exit 1
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

# ---------------------------------------------------------------------------
# Prebuilt binaries
# ---------------------------------------------------------------------------

binaries_dir="${OPENSHELL_PREBUILT_BINARIES_DIR:-}"
if [ -z "$binaries_dir" ]; then
	binaries_dir="${workdir}/binaries"
	mkdir -p "$binaries_dir"
	for binary in openshell openshell-gateway openshell-prover openshell-driver-vm openshell-chutes-edge; do
		printf '#!/bin/sh\necho "%s 0.0.0-verify"\n' "$binary" >"${binaries_dir}/${binary}"
		chmod +x "${binaries_dir}/${binary}"
	done
	echo "==> Using throwaway --version stubs for the package payload"
else
	binaries_dir="$(cd "$binaries_dir" && pwd)"
	for binary in openshell openshell-gateway openshell-prover openshell-driver-vm openshell-chutes-edge; do
		if [ ! -x "${binaries_dir}/${binary}" ]; then
			echo "error: missing or non-executable ${binaries_dir}/${binary}" >&2
			exit 1
		fi
	done
	echo "==> Using prebuilt binaries from ${binaries_dir}"
fi
mkdir -p "$workdir/bin"

# Containers record the binaries by content, and /bin/sh stubs in a bind mount
# are not executed on the host, so only the paths are mounted read-only.

assert_payload_contains() {
	local label="$1"
	local listing_file="$2"
	local missing=0
	local want
	for want in "${PACKAGE_FILES[@]}"; do
		if ! grep -qxF "$want" "$listing_file"; then
			echo "FAIL: ${label} payload is missing /${want}" >&2
			missing=1
		fi
	done
	if [ "$missing" -ne 0 ]; then
		echo "--- ${label} payload ---" >&2
		cat "$listing_file" >&2
		return 1
	fi
	echo "==> ${label} payload contains ${#PACKAGE_FILES[@]} expected files"
}

# ---------------------------------------------------------------------------
# Debian
# ---------------------------------------------------------------------------

if [ "$build_deb" = true ]; then
	echo "==> Building the Debian package in ${DEB_IMAGE}"
	deb_out="${workdir}/deb"
	mkdir -p "$deb_out"
	"$engine" run --rm \
		-v "${repo_root}:/work" \
		-v "${binaries_dir}:/binaries:ro" \
		-v "${deb_out}:/out" \
		-e OPENSHELL_DEB_VERSION="$DEB_VERSION" \
		-e OPENSHELL_DEB_ARCH=amd64 \
		-e OPENSHELL_OUTPUT_DIR=/out \
		"$DEB_IMAGE" bash -lc '
			set -euo pipefail
			OPENSHELL_CLI_BINARY=/binaries/openshell \
			OPENSHELL_GATEWAY_BINARY=/binaries/openshell-gateway \
			OPENSHELL_PROVER_BINARY=/binaries/openshell-prover \
			OPENSHELL_DRIVER_VM_BINARY=/binaries/openshell-driver-vm \
			OPENSHELL_CHUTES_EDGE_BINARY=/binaries/openshell-chutes-edge \
			bash /work/tasks/scripts/package-deb.sh >/tmp/package-deb.log 2>&1 || {
				cat /tmp/package-deb.log >&2
				exit 1
			}
			set +e
			deb=$(find /out -maxdepth 1 -name "*.deb" -type f | head -n 1)
			set -e
			if [ -z "$deb" ]; then
				echo "error: package-deb.sh produced no .deb" >&2
				exit 1
			fi
			dpkg-deb --fsys-tarfile "$deb" | tar -tf - | sed "s|^\./||" | grep -v "^$"
		' >"${workdir}/deb.list"
	assert_payload_contains "deb" "${workdir}/deb.list"
fi

# ---------------------------------------------------------------------------
# RPM
# ---------------------------------------------------------------------------

if [ "$build_rpm" = true ]; then
	rpm_version="${OPENSHELL_RPM_VERSION:-$(sed -n 's/^%global openshell_version //p' openshell.spec)}"
	if [ -z "$rpm_version" ]; then
		echo "error: cannot read %global openshell_version from openshell.spec" >&2
		exit 1
	fi

	echo "==> Building the RPM in ${RPM_IMAGE}"
	# packit vendors the crate tree, writes source tarballs into the tree, and
	# rewrites the spec in place, so it runs against a throwaway copy instead of
	# the checkout.
	"$engine" run --rm \
		-v "${repo_root}:/src:ro" \
		-v "${binaries_dir}:/binaries:ro" \
		-e OPENSHELL_RPM_VERSION="$rpm_version" \
		-e OPENSHELL_RPM_RELEASE="$RPM_RELEASE" \
		-e OPENSHELL_PREBUILT_BINARIES_DIR=/binaries \
		"$RPM_IMAGE" bash -lc '
			set -euo pipefail
			dnf install -y packit rpm-build cargo cargo-rpm-macros git-core \
				pandoc python3-devel systemd-rpm-macros >/tmp/dnf.log 2>&1 || {
				cat /tmp/dnf.log >&2
				exit 1
			}
			mkdir -p /work
			tar -C /src -cf - --exclude=./target --exclude=./x86_64 --exclude=./artifacts . | tar -C /work -xf -
			git config --global --add safe.directory /work
			git config --global user.email verify@example.invalid
			git config --global user.name verify
			cd /work
			packit build locally >/tmp/packit.log 2>&1 || {
				tail -n 40 /tmp/packit.log >&2
				exit 1
			}
			rpm_file=$(find /work -maxdepth 2 -name "openshell-gateway-*.rpm" ! -name "*.src.rpm" -type f | head -n 1)
			if [ -z "$rpm_file" ]; then
				echo "error: packit produced no openshell-gateway RPM" >&2
				exit 1
			fi
			rpm -qlp "$rpm_file" | sed "s|^/||"
		' >"${workdir}/rpm.list"
	assert_payload_contains "rpm" "${workdir}/rpm.list"
fi

echo "package verification passed"

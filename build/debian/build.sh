#!/bin/bash

# This is a script to build a Debian image that can run in a VM created via AVF.
# TODOs:
# - Support x86_64 architecture
# - Add Android-specific packages via a new class
# - Use a stable release from debian-cloud-images

show_help() {
	echo Usage: $0 [OPTION]... [FILE]
	echo Builds a debian image and save it to FILE.
	echo Options:
	echo -h         Pring usage and this help message and exit.
}

check_sudo() {
	if [ "$EUID" -ne 0 ]; then
		echo "Please run as root."
		exit
	fi
}

parse_options() {
	while getopts ":h" option; do
		case ${option} in
			h)
				show_help
				exit;;
		esac
	done
	if [ -n "$1" ]; then
		built_image=$1
	fi
}

install_prerequisites() {
	apt install --no-install-recommends --assume-yes \
		ca-certificates \
		debsums \
		dosfstools \
		fai-server \
		fai-setup-storage \
		fdisk \
		make \
		python3 \
		python3-libcloud \
		python3-marshmallow \
		python3-pytest \
		python3-yaml \
		qemu-utils \
		udev
}

download_debian_cloud_image() {
	local ver=master
	local prj=debian-cloud-images
	local url=https://salsa.debian.org/cloud-team/${prj}/-/archive/${ver}/${prj}-${ver}.tar.gz
	local outdir=${debian_cloud_image}

	mkdir -p ${outdir}
	wget -O - ${url} | tar xz -C ${outdir} --strip-components=1
}

run_fai() {
	local ver=bookworm
	local cspace=${debian_cloud_image}/config_space/${ver}
	local out=${built_image}

	fai-diskimage \
		--verbose \
		--size 2G \
		--class BASE,DEBIAN,NOCLOUD,ARM64,LINUX_VERSION_BASE+LINUX_VARIANT_CLOUD,BOOKWORM,BUILD_IMAGE,SYSTEM_BOOT \
		--cspace ${cspace} \
		${out}
}

clean_up() {
	rm -rf ${workdir}
}

set -e
trap clean_up EXIT

built_image=image.raw
workdir=$(mktemp -d)
debian_cloud_image=${workdir}/debian_cloud_image

check_sudo
parse_options $@
install_prerequisites
download_debian_cloud_image
run_fai

#!/usr/bin/env bash
# Builds the KVM base image a Paygress CI provider serves: an Ubuntu cloud
# image carrying docker, act and git — what ngit-ci's job script expects to
# find in a sandbox.
#
# Every VM the KVM backend creates is a copy-on-write overlay of one base
# image, and that backend ignores the per-workload image a Docker template
# names. So the CI toolchain is baked in here, once, rather than installed per
# job.
#
# Needs `virt-customize` (libguestfs-tools) and `curl`. Building needs no KVM;
# serving the result does.
#
#   ./build.sh
#   ./build.sh --output /var/lib/paygress/vm/base/ci-sandbox.qcow2
#
# Then point the provider's config at it:
#
#   "kvm_base_image_path": "/var/lib/paygress/vm/base/ci-sandbox.qcow2"
#
# or publish the file over HTTP and set `kvm_base_image_url` instead, which the
# provider downloads on first spawn.
#
# Known cost: act pulls its runner platform image on first use, and every job
# gets a fresh VM, so that pull repeats per job. Pre-seeding it into the
# image's docker storage needs a boot-and-snapshot step this script does not
# do yet.

set -euo pipefail

# ngit-ci requires act >= 0.2.86.
ACT_VERSION="v0.2.89"
SOURCE_URL="https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img"
OUTPUT="paygress-ci-sandbox.qcow2"

usage() {
    sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --output) OUTPUT="$2"; shift 2 ;;
        --act-version) ACT_VERSION="$2"; shift 2 ;;
        --source-url) SOURCE_URL="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 1 ;;
    esac
done

for tool in virt-customize curl; do
    command -v "$tool" >/dev/null || {
        echo "$tool is required (apt-get install libguestfs-tools curl)" >&2
        exit 1
    }
done

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

source_image="$workdir/source.img"
echo "==> fetching $SOURCE_URL"
curl -fSL --progress-bar -o "$source_image" "$SOURCE_URL"

# Pinned release artifact rather than the install script, so the image records
# exactly which act it carries and the guest needs no network for this step.
echo "==> fetching act $ACT_VERSION"
curl -fSL "https://github.com/nektos/act/releases/download/${ACT_VERSION}/act_Linux_x86_64.tar.gz" \
    | tar -xzf - -C "$workdir" act

mkdir -p "$(dirname "$OUTPUT")"
cp "$source_image" "$OUTPUT"

echo "==> installing the CI toolchain"
virt-customize -a "$OUTPUT" \
    --install docker.io,git,curl,ca-certificates,jq \
    --upload "$workdir/act:/usr/local/bin/act" \
    --run-command 'chmod 0755 /usr/local/bin/act' \
    --run-command 'systemctl enable docker' \
    --run-command 'systemctl enable ssh' \
    --run-command 'cloud-init clean --logs || true'

echo
echo "built $OUTPUT"
ls -lh "$OUTPUT"
if command -v sha256sum >/dev/null; then
    sha256sum "$OUTPUT"
fi

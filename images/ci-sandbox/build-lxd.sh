#!/usr/bin/env bash
# Builds the LXD image a Paygress CI provider serves: Ubuntu with docker, act
# and git — what ngit-ci's job script expects to find in a sandbox.
#
# The LXD backend launches every instance with `security.nesting=true`, which
# is what lets a job run its own Docker daemon without the host's socket or a
# privileged container. Nesting is set per instance at launch, so it is not
# something this image has to carry.
#
# Run on the provider host (the image is published to its local LXD).
#
#   ./build-lxd.sh
#   ./build-lxd.sh --alias paygress-ci --no-seed
#
# Then spawn against it with no template, since a template would name a Docker
# image LXD cannot launch:
#
#   paygress-cli adapter --image paygress-ci ...
#
# By default the act runner platform image is pulled into the image's docker
# storage, which costs a few GB and a few minutes here but saves every job the
# same pull. `--no-seed` skips it.

set -euo pipefail

ALIAS="paygress-ci"
BASE_IMAGE="ubuntu:24.04"
# ngit-ci requires act >= 0.2.86.
ACT_VERSION="v0.2.89"
# ngit-ci's default act platform mapping is the catthehacker medium images.
SEED_IMAGE="catthehacker/ubuntu:act-24.04"
BUILDER="paygress-ci-build"

usage() {
    sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --alias) ALIAS="$2"; shift 2 ;;
        --base-image) BASE_IMAGE="$2"; shift 2 ;;
        --act-version) ACT_VERSION="$2"; shift 2 ;;
        --seed-image) SEED_IMAGE="$2"; shift 2 ;;
        --no-seed) SEED_IMAGE=""; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 1 ;;
    esac
done

command -v lxc >/dev/null || { echo "lxc is required" >&2; exit 1; }

cleanup() {
    lxc delete -f "$BUILDER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> launching $BUILDER from $BASE_IMAGE"
cleanup
lxc launch "$BASE_IMAGE" "$BUILDER" -c security.nesting=true
lxc exec "$BUILDER" -- cloud-init status --wait >/dev/null

echo "==> installing docker, git and act $ACT_VERSION"
lxc exec "$BUILDER" -- bash -eu -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq docker.io git curl ca-certificates jq openssh-server
    curl -fsSL 'https://github.com/nektos/act/releases/download/${ACT_VERSION}/act_Linux_x86_64.tar.gz' \
        | tar -xz -C /usr/local/bin act
    chmod 0755 /usr/local/bin/act
    systemctl enable docker ssh
"

if [ -n "$SEED_IMAGE" ]; then
    echo "==> seeding $SEED_IMAGE into the image's docker storage"
    lxc exec "$BUILDER" -- bash -eu -c "
        systemctl start docker
        for i in \$(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 2; done
        docker pull '$SEED_IMAGE'
    "
fi

echo "==> verifying the toolchain"
lxc exec "$BUILDER" -- bash -eu -c '
    docker --version
    act --version
    git --version
'

echo "==> cleaning instance identity"
lxc exec "$BUILDER" -- bash -eu -c '
    # Host keys are per instance, not per image. sshd regenerates them from the
    # drop-in below rather than relying on cloud-init having run first, because
    # a missing key means no sshd and an unreachable sandbox.
    mkdir -p /etc/systemd/system/ssh.service.d
    printf "[Service]\nExecStartPre=/usr/bin/ssh-keygen -A\n" \
        > /etc/systemd/system/ssh.service.d/10-paygress-genkeys.conf
    rm -f /etc/ssh/ssh_host_*

    apt-get clean
    rm -rf /var/lib/apt/lists/*
    cloud-init clean --logs || true
    truncate -s 0 /etc/machine-id
'

echo "==> publishing as $ALIAS"
lxc stop "$BUILDER"
lxc publish "$BUILDER" --alias "$ALIAS" --reuse

echo
lxc image list "$ALIAS" -c lfsu
echo "spawn against it with: paygress-cli adapter --image $ALIAS ..."

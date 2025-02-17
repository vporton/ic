#!/usr/bin/env bash

# This script verifies a specific commit hash or a proposal for reproducibility.
# if it's a proposal we need to make sure that proposal_hash == CDN_hash == build_hash
# otherwise we only need to make sure that CDN_hash == build_hash.

set -euo pipefail

pushd() {
    command pushd "$@" >/dev/null
}

popd() {
    command popd "$@" >/dev/null
}

print_red() {
    echo -e "\033[0;31m$(date +'%Y/%m/%d | %H:%M:%S | %s') $*\033[0m" 1>&2
}

print_green() {
    echo -e "\033[0;32m$(date +'%Y/%m/%d | %H:%M:%S | %s') $*\033[0m"
}

print_blue() {
    echo -e "\033[0;34m$(date +'%Y/%m/%d | %H:%M:%S | %s') $*\033[0m"
}

print_purple() {
    echo -e "\033[0;35m$(date +'%Y/%m/%d | %H:%M:%S | %s') $*\033[0m"
}

log() {
    print_blue "[+] $*"
}

log_success() {
    print_green "[+] $*"
}

log_stderr() {
    print_red "[-] $*"
}

log_debug() {
    if [ -n "${DEBUG:-}" ]; then
        print_purple "[_] $*"
    fi
}

error() {
    print_red "[-] $1"
    exit 1
}

print_usage() {
    cat >&2 <<-USAGE
    This script builds and diffs the update image between CI and build-ic
    Pick one of the following options:
    -h	    this help message
    -p      proposal id to check - the proposal has to be for an update-img
    -c	    git revision/commit to use - the commit has to exist on master branch of
            the IC repository on GitHub and you should be running the script
            from that branch
    <empty> no option - uses the commit at the tip of the branch this is run on
USAGE
}

extract_field_json() {
    jq_field="$1"
    input="$2"

    out=$(cat "$input" | jq --raw-output "$jq_field")
    status="$?"

    if [[ "$status" != 0 ]] || [[ "$out" == "null" ]]; then
        error "Field $jq_field does not exist in $input"
    fi

    echo "$out"
}

check_git_repo() {
    log_debug "Check we are inside a Git repository"
    if [ "$(git rev-parse --is-inside-work-tree 2>/dev/null)" != "true" ]; then
        error "Please run this script inside of a git repository"
    fi
}

check_ic_repo() {
    git_remote="$(git config --get remote.origin.url)"

    log_debug "Check the repository is an IC repository"
    # Possible values of `git_remote` are listed below
    # git@gitlab.com:dfinity-lab/public/ic.git
    # git@github.com:dfinity/ic.git
    # https://github.com/dfinity/ic.git
    if [ "$git_remote" != "*/ic.git" ]; then
        error "When not specifying any option please run this script inside an IC git repository"
    fi
}

#################### Set-up
if [ "${DEBUG:-}" == "2" ]; then
    set -x
fi

proposal_id=""
git_commit=""
no_option=""
SECONDS=0
pwd="$(pwd)"

# Parse arguments
while getopts ':i:h:c:p:d:' flag; do
    case "${flag}" in
        c) git_commit="${OPTARG}" ;;
        p) proposal_id="${OPTARG}" ;;
        *)
            print_usage
            exit 1
            ;;
    esac
done

log "Check the environment"
# either of those files should exist
source /usr/lib/os-release 2>/dev/null
source /etc/os-release 2>/dev/null

if [ "$(uname -m)" == "x86_64" ]; then
    log_success "x86_64 architecture detected"
else
    error "Please run this script on x86_64 architecture"
fi

if [ "${NAME:-}" == "Ubuntu" ]; then
    log_success "Ubuntu OS detected"
else
    error "Please run this script on Ubuntu OS"
fi

if [[ $(echo "${VERSION_ID:-} > 22.03" | bc) == 1 ]]; then
    log_success "Version >22.04 detected"
else
    error "Please run this script on Ubuntu version 22.04 or higher"
fi

if [[ "$(cat /proc/meminfo | grep MemTotal | awk '{ print int($2/1024**2) }')" -ge 15 ]]; then
    log_success "More than 16GB of RAM detected"
else
    error "You need at least 16GB of RAM on this machine"
fi

if [[ $(("$(df . --output=avail | tail -n 1)" / 1000000)) -ge 100 ]]; then
    log_success "More than 100GB of free disk space detected"
else
    error "You need at least 100GB of free disk space on this machine"
fi

log "Update package registry"
sudo apt-get update -y
log "Install needed dependencies"
sudo apt-get install git curl jq podman -y

# if no options have been choosen we assume to check the latest commit of the
# branch we are on.
if [ "$OPTIND" -eq 1 ]; then
    check_git_repo
    check_ic_repo

    no_option="true"
fi

# Check `git_commit` exists on the master branch of the IC repository on GitHub
if [ -n "$git_commit" ]; then
    check_git_repo
    check_ic_repo

    if [ -z "$(git branch master --contains $git_commit)" ]; then
        error "When specifying the -c option please specify a hash which exists on the master branch of the IC repository"
    fi
fi

# set the `git_hash` from the `proposal_id` or from the environment
if [ -n "$proposal_id" ]; then

    # format the proposal
    proposal_url="https://ic-api.internetcomputer.org/api/v3/proposals/$proposal_id"
    proposal_body="proposal-body.json"

    log_debug "Fetch the proposal json body"
    proposal_body_status=$(curl --silent --show-error -w %{http_code} --location --retry 5 --retry-delay 10 "$proposal_url" -o "$proposal_body")

    # check for error
    if ! [[ "$proposal_body_status" =~ ^2 ]]; then
        error "Could not fetch $proposal_id, please make sure you have a valid internet connection or that the proposal #$proposal_id exists"
    fi
    log_debug "Extract the package_url"
    proposal_package_url=$(extract_field_json ".payload.release_package_urls[0]" "$proposal_body")

    log_debug "Extract the sha256 sums hex for the artifacts from the proposal"
    proposal_package_sha256_hex=$(extract_field_json ".payload.release_package_sha256_hex" "$proposal_body")

    log_debug "Extract git_hash out of the proposal"
    git_hash=$(extract_field_json ".payload.replica_version_to_elect" "$proposal_body")

else

    log_debug "Extract git_hash from CLI arguments or directory's HEAD"
    git_hash=${git_commit:-$(git rev-parse HEAD)}
fi

tmpdir="$(mktemp -d)"
log "Set our working directory to a temporary one - $tmpdir"

# if we are in debug mode we keep the directories to debug any issues
if [ -z "${DEBUG:-}" ]; then
    trap 'rm -rf "$tmpdir"' EXIT
fi

pushd "$tmpdir"

log "Set and create output directories for the different images"
out="$tmpdir/disk-images/$git_hash"
log "Images will be saved in $out"

ci_out="$out/ci-img"
dev_out="$out/dev-img"
proposal_out="$out/proposal-img"

mkdir -p "$ci_out"
mkdir -p "$dev_out"
mkdir -p "$proposal_out"

#################### Check Proposal Hash
# download and check the hash matches
if [ -n "$proposal_id" ]; then

    log "Check the proposal url is correctly formatted"
    expected_url="https://download.dfinity.systems/ic/$git_hash/guest-os/update-img/update-img.tar.gz"
    if [ "$proposal_package_url" != "$expected_url" ]; then
        error "The artifact's URL is wrongly formatted, please report this to DFINITY\n\t\tcurrent  = $proposal_package_url\n\t\texpected = $expected_url"
    fi

    log "Download the proposal artifacts"
    curl --silent --show-error --location --retry 5 --retry-delay 10 \
        --remote-name --output-dir "$proposal_out" "$proposal_package_url"

    pushd "$proposal_out"

    log "Check the hash of the artifacts is the correct one"
    echo "$proposal_package_sha256_hex  update-img.tar.gz" | shasum -a256 -c- >/dev/null

    log_success "The proposal's artefacts and hash match"
    popd
fi

#################### Check CI Hash
log "Downloads the image version built and pushed by CI system"
curl --silent --show-error --location --retry 5 --retry-delay 10 --remote-name --output-dir "$ci_out" "https://download.dfinity.systems/ic/$git_hash/guest-os/update-img/update-img.tar.gz"
curl --silent --show-error --location --retry 5 --retry-delay 10 --remote-name --output-dir "$ci_out" "https://download.dfinity.systems/ic/$git_hash/guest-os/update-img/SHA256SUMS"

log "Check the hash upload matches with the image uploaded"
pushd "$ci_out"
grep "update-img.tar.gz" SHA256SUMS | shasum -a256 -c- >/dev/null
log_success "The CI's artefacts and hash match"

# extract the hash from the SHA256SUMS file
ci_package_sha256_hex="$(grep update-img.tar.gz SHA256SUMS | cut -d' ' -f 1)"

popd

#################### Verify Proposal Image == CI Image
log "Check the shasum that was set in the proposal matches the one we download from CDN"
if [ -n "$proposal_id" ]; then
    if [ "$proposal_package_sha256_hex" != "$ci_package_sha256_hex" ]; then
        error "The sha256 sum from the proposal does not match the one from the CDN storage for udpate-img.tar.gz. The sha256 sum from the proposal: $proposal_package_sha256_hex The sha256 sum from the CDN storage: $ci_package_sha256_hex."
    else
        log_success "The shasum from the proposal and CDN match"
    fi
fi

################### Verify CI Image == Dev Image
# Copy if we are in CI, if there wasn't an option specified or if it was `git_commit`
if [ -n "${CI:-}" ] || [ -n "$no_option" ] || [ -n "$git_commit" ]; then
    log "Copy IC repository from $pwd to temporary directory"
    git clone --depth 1 "$pwd" .
else
    log "Clone IC repository"
    git clone https://github.com/dfinity/ic
fi

pushd ic
log "Check out $git_hash commit"
git fetch --quiet origin "$git_hash"
git checkout --quiet "$git_hash"

log "Build IC-OS"
./gitlab-ci/container/build-ic.sh --icos >/dev/null
log_success "Built IC-OS successfully"

mv artifacts/icos/guestos/update-img.tar.gz "$dev_out"

log "Check hash of locally built artifact matches the one fetched from the proposal/CDN"
pushd "$dev_out"
dev_package_sha256_hex="$(shasum -a 256 "update-img.tar.gz" | cut -d' ' -f1)"

if [ "$dev_package_sha256_hex" != "$ci_package_sha256_hex" ]; then
    log_stderr "The sha256 sum from the proposal/CDN does not match the one from we just built. \n\tThe sha256 sum we just built:\t\t$dev_package_sha256_hex\n\tThe sha256 sum from the CDN: $ci_package_sha256_hex."

    if [ -n "${INVESTIGATE:-}" ]; then

        log "Start investigation of build build non-reproducibility"
        sudo apt-get install diffoscope -y

        log "Extract images"
        tar xzf "$ci_out/update-img.tar.gz" -C "$ci_out"
        tar xzf "$dev_out/update-img.tar.gz" -C "$dev_out"

        log "Run diffoscope"
        sudo diffoscope "$ci_out/boot.img" "$dev_out/boot.img" || true
        sudo diffoscope "$ci_out/root.img" "$dev_out/root.img" || true

        log "Disk images saved to $out"
    else
        exit 1
    fi
else
    log_success "The shasum from the artifact built locally and the one fetched from the proposal/CDN match.\n\t\t\t\t\t\tLocal = $dev_package_sha256_hex\n\t\t\t\t\t\tCDN   = $ci_package_sha256_hex"
    log_success "Verification successful - total time: $(($SECONDS / 3600))h $((($SECONDS / 60) % 60))m $(($SECONDS % 60))s"

fi

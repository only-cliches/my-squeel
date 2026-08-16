#!/usr/bin/env bash
set -euo pipefail

MARIADB_IMAGE="${MARIADB_MTR_IMAGE:-mariadb:10.11.7}"
UBUNTU_IMAGE="${MARIADB_MTR_UBUNTU_IMAGE:-ubuntu:24.04}"
MTR_PLATFORM="linux/arm64"
MARIADB_PASSWORD="my-sqweel"
MARIADB_DATABASE="test"
MARIADB_CONTAINER="my-sqweel-mtr-mariadb-$$"
MTR_NETWORK="my-sqweel-mtr-$$"
NETWORK_CREATED="false"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: tools/run_mariadb_mtr_baseline_docker.sh

Run the pinned MariaDB 10.11.7 MTR baseline in ARM64 Docker containers.
Reports are written to artifacts/mariadb-mtr-baseline.
EOF
}

cleanup() {
  docker rm --force "$MARIADB_CONTAINER" >/dev/null 2>&1 || true
  if [[ "$NETWORK_CREATED" == "true" ]]; then
    docker network rm "$MTR_NETWORK" >/dev/null 2>&1 || true
  fi
}

case "${1:-}" in
  "") ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

if ! command -v docker >/dev/null 2>&1; then
  echo "MariaDB MTR baseline requires Docker Desktop or another working Docker installation" >&2
  exit 1
fi
if ! docker info >/dev/null 2>&1; then
  echo "Docker is installed but its daemon is unavailable" >&2
  exit 1
fi

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Creating isolated Docker network: $MTR_NETWORK"
docker network create "$MTR_NETWORK" >/dev/null
NETWORK_CREATED="true"

echo "Starting MariaDB baseline: $MARIADB_IMAGE"
docker run \
  --detach \
  --rm \
  --platform "$MTR_PLATFORM" \
  --network "$MTR_NETWORK" \
  --name "$MARIADB_CONTAINER" \
  --env "MARIADB_ROOT_PASSWORD=$MARIADB_PASSWORD" \
  --env "MARIADB_DATABASE=$MARIADB_DATABASE" \
  "$MARIADB_IMAGE" >/dev/null

mariadb_ready="false"
for ((attempt = 1; attempt <= 90; attempt++)); do
  if docker exec "$MARIADB_CONTAINER" \
    mariadb-admin ping --silent --host 127.0.0.1 --password="$MARIADB_PASSWORD" \
    >/dev/null 2>&1; then
    mariadb_ready="true"
    break
  fi
  sleep 1
done
if [[ "$mariadb_ready" != "true" ]]; then
  echo "MariaDB did not become ready within 90 seconds" >&2
  docker logs "$MARIADB_CONTAINER" >&2 || true
  exit 1
fi

echo "Running the pinned MTR baseline in $UBUNTU_IMAGE ($MTR_PLATFORM)"
docker run \
  --rm \
  --platform "$MTR_PLATFORM" \
  --network "$MTR_NETWORK" \
  --volume "$REPOSITORY_ROOT:/workspace" \
  --workdir /workspace \
  --env "MARIADB_COMPARE_URL=mysql://root:$MARIADB_PASSWORD@$MARIADB_CONTAINER:3306/$MARIADB_DATABASE" \
  "$UBUNTU_IMAGE" \
  bash -lc '
    set -euo pipefail

    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      libconfig-inifiles-perl \
      libedit2 \
      libncurses6 \
      libnuma1 \
      libpcre2-8-0 \
      libpcre2-posix3 \
      libpmem1 \
      libssl3t64 \
      libtinfo6 \
      liburing2 \
      mysql-common \
      perl \
      python3 \
      zlib1g

    eval "$(tools/prepare_mariadb_mtr.sh .cache/mariadb-mtr --print-env)"

    python3 tools/mariadb_mtr_compat.py \
      --target baseline \
      --suite-root "$MARIADB_MTR_ROOT" \
      --allowlist tests/mariadb-mtr-allowlist.txt \
      --baseline-url "$MARIADB_COMPARE_URL" \
      --mysqltest-bin "$MYSQLTEST_BIN" \
      --client-bindir "$MYSQL_CLIENT_BINDIR" \
      --mtr-runner "$MTR_RUNNER" \
      --safe-process-bin "$MTR_SAFE_PROCESS" \
      --mtr-layout mariadb \
      --baseline-label MariaDB \
      --report-dir artifacts/mariadb-mtr-baseline \
      --baseline-version 10.11.7 \
      --source-revision 10.11.7-2ubuntu2 \
      --minimum-percent 100
  '

echo "MariaDB MTR baseline passed"
echo "Report: $REPOSITORY_ROOT/artifacts/mariadb-mtr-baseline/mtr-report.md"

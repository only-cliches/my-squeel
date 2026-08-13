#!/usr/bin/env bash
set -euo pipefail

MYSQL_DOCKER_IMAGE="mysql:8.0.43"
MYSQL_DOCKER_PASSWORD="my-sqweel"
MYSQL_DOCKER_DATABASE="test"
MYSQL_CONTAINER=""

cleanup() {
  if [[ -n "$MYSQL_CONTAINER" ]]; then
    docker rm -f "$MYSQL_CONTAINER" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT

minimum_open_files=8192
current_open_files="$(ulimit -Sn)"
if [[ "$current_open_files" != "unlimited" ]] \
  && (( current_open_files < minimum_open_files )); then
  if ! ulimit -Sn "$minimum_open_files"; then
    echo "could not raise the open-file limit from $current_open_files to $minimum_open_files" >&2
    exit 1
  fi
fi

if [[ -z "${MYSQL_COMPARE_URL:-}" ]]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "real-MySQL parity requires MYSQL_COMPARE_URL or a working Docker installation" >&2
    exit 1
  fi
  if ! docker info >/dev/null 2>&1; then
    echo "Docker is installed but its daemon is unavailable" >&2
    exit 1
  fi
  if ! docker image inspect "$MYSQL_DOCKER_IMAGE" >/dev/null 2>&1; then
    echo "Pulling the CI comparison image: $MYSQL_DOCKER_IMAGE"
    docker pull "$MYSQL_DOCKER_IMAGE"
  fi

  MYSQL_CONTAINER="my-sqweel-prepush-$$"
  echo "Starting the CI comparison image: $MYSQL_DOCKER_IMAGE"
  docker run \
    --detach \
    --rm \
    --name "$MYSQL_CONTAINER" \
    --env "MYSQL_ROOT_PASSWORD=$MYSQL_DOCKER_PASSWORD" \
    --env "MYSQL_DATABASE=$MYSQL_DOCKER_DATABASE" \
    --publish 127.0.0.1::3306 \
    "$MYSQL_DOCKER_IMAGE" >/dev/null

  mysql_port="$(docker port "$MYSQL_CONTAINER" 3306/tcp | head -n 1)"
  mysql_port="${mysql_port##*:}"
  if [[ ! "$mysql_port" =~ ^[0-9]+$ ]]; then
    echo "could not determine the published MySQL port" >&2
    exit 1
  fi

  mysql_ready="false"
  for _ in $(seq 1 90); do
    if docker exec "$MYSQL_CONTAINER" \
      mysqladmin ping --silent --host 127.0.0.1 --password="$MYSQL_DOCKER_PASSWORD" \
      >/dev/null 2>&1; then
      mysql_ready="true"
      break
    fi
    sleep 1
  done
  if [[ "$mysql_ready" != "true" ]]; then
    echo "MySQL did not become ready within 90 seconds" >&2
    docker logs "$MYSQL_CONTAINER" >&2 || true
    exit 1
  fi

  export MYSQL_COMPARE_URL="mysql://root:$MYSQL_DOCKER_PASSWORD@127.0.0.1:$mysql_port/$MYSQL_DOCKER_DATABASE"
fi

echo "Running compatibility-report tooling tests"
python3 -m unittest discover -s tests -p 'test_*.py'

echo "Running all targets with real-MySQL parity required"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
MYSQL_PARITY_REQUIRED=1 cargo test --all-targets --locked

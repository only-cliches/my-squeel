#!/usr/bin/env bash
set -euo pipefail

# Fetch the pinned upstream test source and ensure that the MTR client tools
# are available. MySQL's test sources are not vendored in this repository.

ROOT="${1:?usage: prepare_mysql_mtr.sh ROOT [--print-env]}"
PRINT_ENV="${2:-}"
TAG="${MYSQL_MTR_TAG:-mysql-8.0.43}"
REVISION="${MYSQL_MTR_REVISION:-2d6d5e10436a8f2b58d37af737c2a3e45855d0b7}" # overridden below if tag differs

run_logged() {
  if [[ "$PRINT_ENV" == "--print-env" ]]; then
    "$@" >&2
  else
    "$@"
  fi
}

if [[ "$TAG" == "mysql-8.0.43" ]]; then
  REVISION="2d6d5e10436a8f2b58d37af737c2a3e45855d0b7"
fi

if [[ ! -d "$ROOT/.git" ]]; then
  mkdir -p "$(dirname "$ROOT")"
  run_logged git clone --depth 1 --branch "$TAG" https://github.com/mysql/mysql-server.git "$ROOT"
fi

actual_revision="$(git -C "$ROOT" rev-parse HEAD)"
if [[ "$actual_revision" != "$REVISION" ]]; then
  echo "unexpected MySQL source revision: $actual_revision (wanted $REVISION)" >&2
  exit 1
fi

if [[ -n "${MYSQLTEST_BIN:-}" ]]; then
  mysqltest_bin="$MYSQLTEST_BIN"
elif command -v mysqltest >/dev/null 2>&1; then
  mysqltest_bin="$(command -v mysqltest)"
else
  build_dir="${MYSQL_MTR_BUILD_DIR:-$ROOT/build}"
  boost_dir="${MYSQL_MTR_BOOST_DIR:-$ROOT/boost}"
  run_logged cmake -S "$ROOT" -B "$build_dir" \
    -DCMAKE_BUILD_TYPE=Release \
    -DDOWNLOAD_BOOST=1 \
    -DWITH_BOOST="$boost_dir" \
    -DWITH_DEBUG=OFF \
    -DWITH_NDB=OFF \
    -DWITH_ROUTER=OFF \
    -DWITH_UNIT_TESTS=OFF \
    -DWITH_SSL=system \
    -DWITH_ZLIB=system \
    -DWITH_LZ4=system \
    -DWITH_ZSTD=system \
    -DWITH_PROTOBUF=system
  # MTR's external-server mode still uses the client-side helpers below to
  # launch tests and inspect the server.  Build them explicitly so a fresh CI
  # checkout does not fail during MTR setup with a misleading missing-tool
  # error.
  run_logged cmake --build "$build_dir" \
    --target mysql mysqltest mysqltest_safe_process mysql_migrate_keyring \
    mysql_ssl_rsa_setup \
    mysql_keyring_encryption_test mysqladmin mysqlbinlog mysqlpump mysql_upgrade \
    mysqlcheck mysqlshow mysqldump my_print_defaults innochecksum ibd2sdi \
    myisamchk myisampack perror mysql_tzinfo_to_sql \
    --parallel "${CMAKE_BUILD_PARALLEL_LEVEL:-2}"
  mysqltest_bin="$(find "$build_dir" -type f -name mysqltest -perm -111 -print -quit)"
fi

if [[ -z "$mysqltest_bin" || ! -x "$mysqltest_bin" ]]; then
  echo "mysqltest binary is unavailable; set MYSQLTEST_BIN or install/build MySQL client tools" >&2
  exit 1
fi

client_bindir="$(dirname "$(realpath "$mysqltest_bin")")"
if [[ ! -x "$client_bindir/mysql" ]]; then
  echo "mysql client is unavailable beside mysqltest: $client_bindir/mysql" >&2
  exit 1
fi

for required_tool in mysqladmin mysqlbinlog mysqlpump mysql_upgrade mysql_ssl_rsa_setup \
  mysql_migrate_keyring mysql_keyring_encryption_test mysqltest_safe_process mysqlcheck \
  mysqlshow mysqldump my_print_defaults innochecksum ibd2sdi myisamchk myisampack perror \
  mysql_tzinfo_to_sql; do
  if [[ ! -x "$client_bindir/$required_tool" ]]; then
    echo "required MTR helper is unavailable beside mysqltest: $client_bindir/$required_tool" >&2
    exit 1
  fi
done

if [[ "$PRINT_ENV" == "--print-env" ]]; then
  printf 'MYSQL_MTR_ROOT=%q\n' "$ROOT"
  printf 'MYSQLTEST_BIN=%q\n' "$mysqltest_bin"
  printf 'MYSQL_CLIENT_BINDIR=%q\n' "$client_bindir"
else
  echo "MySQL MTR source: $ROOT"
  echo "mysqltest: $mysqltest_bin"
  echo "client bindir: $client_bindir"
fi

#!/usr/bin/env bash
set -euo pipefail

# Fetch and extract Oracle's pinned MySQL test, client, and server packages.
# The Ubuntu packages contain mysql-test-run.pl, mysqltest, and every helper
# MTR initializes even when it runs against an external server.

ROOT="${1:?usage: prepare_mysql_mtr.sh ROOT [--print-env]}"
PRINT_ENV="${2:-}"
VERSION="8.0.43"
PACKAGE_RELEASE="1ubuntu24.04"
PACKAGE_ARCH="amd64"
PACKAGE_BASE_URL="https://repo.mysql.com/apt/ubuntu/pool/mysql-8.0/m/mysql-community"
PACKAGE_CACHE="${MYSQL_MTR_CACHE_DIR:-$ROOT/packages}"

packages=(
  "dcd97b70feb6932d40f4c08ea53868379c3ecf428e2db69d5f74d8b46e4e8a66 mysql-common_${VERSION}-${PACKAGE_RELEASE}_${PACKAGE_ARCH}.deb"
  "b82626a43e2853f74319d9e9202234cf77873e7db308211cf3daaa268e4c27cd mysql-community-client-plugins_${VERSION}-${PACKAGE_RELEASE}_${PACKAGE_ARCH}.deb"
  "3cbe9628bad371f605693f07843bc2f17ee5336b4cfa9429309df273e91f4486 mysql-community-client-core_${VERSION}-${PACKAGE_RELEASE}_${PACKAGE_ARCH}.deb"
  "ab67557756cf1d863e4a57c8e4d177fbc40aa65e0377d1e43eca05366d804db1 mysql-community-client_${VERSION}-${PACKAGE_RELEASE}_${PACKAGE_ARCH}.deb"
  "f35f559b2cb0644f3ae6eaee31b4493f3356a600ab20dcba4f78705c06f6b39a mysql-community-server-core_${VERSION}-${PACKAGE_RELEASE}_${PACKAGE_ARCH}.deb"
  "8deaecf8c675f741dc0dcfadc23d1d973c840f806f2c39813d2c9eb20e28fda4 mysql-community-server_${VERSION}-${PACKAGE_RELEASE}_${PACKAGE_ARCH}.deb"
  "d3aa6f3ca5ae9a774126c53c45a9a927e1d097405514ba5df36c36d8429b0a06 mysql-community-test_${VERSION}-${PACKAGE_RELEASE}_${PACKAGE_ARCH}.deb"
)

run_logged() {
  if [[ "$PRINT_ENV" == "--print-env" ]]; then
    "$@" >&2
  else
    "$@"
  fi
}

if [[ "$(dpkg --print-architecture)" != "$PACKAGE_ARCH" ]]; then
  echo "MySQL MTR binary packages require Ubuntu amd64" >&2
  exit 1
fi

mkdir -p "$ROOT" "$PACKAGE_CACHE"
for package_entry in "${packages[@]}"; do
  read -r expected_checksum package_file <<<"$package_entry"
  package_path="$PACKAGE_CACHE/$package_file"
  if [[ -f "$package_path" ]] && \
    echo "$expected_checksum  $package_path" | sha256sum --check --status; then
    continue
  fi

  download_path="$package_path.download"
  run_logged curl --fail --location --retry 3 --silent --show-error \
    --output "$download_path" "$PACKAGE_BASE_URL/$package_file"
  if ! echo "$expected_checksum  $download_path" | sha256sum --check --status; then
    echo "checksum mismatch for MySQL package: $package_file" >&2
    exit 1
  fi
  mv "$download_path" "$package_path"
done

for package_entry in "${packages[@]}"; do
  read -r _ package_file <<<"$package_entry"
  run_logged dpkg-deb --extract "$PACKAGE_CACHE/$package_file" "$ROOT"
done

suite_root="$ROOT/usr/lib"
mysqltest_bin="$ROOT/usr/bin/mysqltest"
client_bindir="$ROOT/usr/bin"
mysqld_bin="$ROOT/usr/sbin/mysqld"

if [[ ! -x "$mysqld_bin" ]]; then
  echo "required MTR server binary is unavailable: $mysqld_bin" >&2
  exit 1
fi
if [[ ! -f "$suite_root/mysql-test/t/select_all.test" ]]; then
  echo "pinned MySQL MTR suite is unavailable: $suite_root/mysql-test" >&2
  exit 1
fi

for required_tool in mysql mysqltest mysqladmin mysqlbinlog mysqlcheck \
  mysql_config_editor mysqlimport mysqlpump mysql_secure_installation mysqlshow \
  mysqlslap mysql_upgrade mysqldump mysql_ssl_rsa_setup mysql_migrate_keyring \
  mysql_keyring_encryption_test mysqltest_safe_process my_print_defaults \
  innochecksum ibd2sdi lz4_decompress zlib_decompress myisamchk myisampack \
  perror mysql_tzinfo_to_sql; do
  if [[ ! -x "$client_bindir/$required_tool" ]]; then
    echo "required MTR helper is unavailable: $client_bindir/$required_tool" >&2
    exit 1
  fi
done

if [[ "$PRINT_ENV" == "--print-env" ]]; then
  printf 'MYSQL_MTR_ROOT=%q\n' "$suite_root"
  printf 'MYSQLTEST_BIN=%q\n' "$mysqltest_bin"
  printf 'MYSQL_CLIENT_BINDIR=%q\n' "$client_bindir"
  printf 'MTR_BINDIR=%q\n' "$ROOT/usr"
else
  echo "MySQL MTR package: $VERSION-$PACKAGE_RELEASE"
  echo "MTR suite: $suite_root/mysql-test"
  echo "mysqltest: $mysqltest_bin"
  echo "client bindir: $client_bindir"
fi

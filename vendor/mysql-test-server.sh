#!/usr/bin/env bash
set -euo pipefail

MYSQL_DATA_DIR="${MYSQL_DATA_DIR:-/tmp/my_sqweel_mysql_test}"
MYSQL_PORT="${MYSQL_TEST_PORT:-3307}"
MYSQL_SOCKET="${MYSQL_TEST_SOCKET:-/tmp/mysql-test.sock}"
MYSQL_PID_FILE="${MYSQL_PID_FILE:-$MYSQL_DATA_DIR/mysqld.pid}"
MYSQL_LOG_FILE="${MYSQL_LOG_FILE:-$MYSQL_DATA_DIR/mysqld.log}"
MYSQL_DATABASE="${MYSQL_DATABASE:-app}"
RUN_COMMAND="false"
STARTED_BY_SCRIPT="false"
CMD=()

MYSQLD_BIN="${MYSQLD_BIN:-}"
if [[ -x "/usr/sbin/mysqld" ]]; then
    MYSQLD_BIN="${MYSQLD_BIN:-/usr/sbin/mysqld}"
elif [[ -x "/usr/sbin/mariadbd" ]]; then
    MYSQLD_BIN="${MYSQLD_BIN:-/usr/sbin/mariadbd}"
elif command -v mysqld >/dev/null 2>&1; then
    MYSQLD_BIN="${MYSQLD_BIN:-$(command -v mysqld)}"
elif command -v mariadbd >/dev/null 2>&1; then
    MYSQLD_BIN="${MYSQLD_BIN:-$(command -v mariadbd)}"
fi

if [[ -z "${MYSQLD_BIN}" ]]; then
    echo "mysql/mariadb server binary not found; install default-mysql-server first." >&2
    exit 1
fi

MYSQLD_USER_ARG=()
if [[ "$(id -u)" -eq 0 ]]; then
    MYSQLD_USER_ARG+=(--user="$(id -un)")
fi

usage() {
    cat <<'USAGE'
Usage:
  mysql-test-server
    Start MySQL in the background and print connection info.

  mysql-test-server --run "cmd" [args...]
    Start MySQL in the background, run a command, then stop MySQL.

  mysql-test-server stop
    Stop a previously started background server for this directory.

Environment:
  MYSQL_DATA_DIR   Data directory (default: /tmp/my_sqweel_mysql_test)
  MYSQL_TEST_PORT  Port (default: 3307)
  MYSQL_TEST_SOCKET Path to socket (default: /tmp/mysql-test.sock)
  MYSQL_DATABASE   Database to create (default: app)
USAGE
}

if [[ "${1-}" == "--help" || "${1-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ "${1-}" == "stop" ]]; then
    if [[ -f "${MYSQL_PID_FILE}" ]]; then
        if kill "$(cat "${MYSQL_PID_FILE}")" 2>/dev/null; then
            echo "Stopped MySQL."
        fi
        rm -f "${MYSQL_PID_FILE}"
    else
        echo "No MySQL pid file at ${MYSQL_PID_FILE}" >&2
        exit 1
    fi
    exit 0
fi

if [[ "${1-}" == "--run" ]]; then
    shift
    CMD=("$@")
    if [[ ${#CMD[@]} -eq 0 ]]; then
        echo "error: --run requires a command to execute" >&2
        exit 1
    fi
    RUN_COMMAND="true"
fi

mkdir -p "${MYSQL_DATA_DIR}"

if [[ ! -d "${MYSQL_DATA_DIR}/mysql" ]]; then
    if ${MYSQLD_BIN} --verbose --help | grep -q -- "--initialize-insecure"; then
        "${MYSQLD_BIN}" --no-defaults --initialize-insecure "${MYSQLD_USER_ARG[@]}" --log-error="${MYSQL_LOG_FILE}" --datadir="${MYSQL_DATA_DIR}"
    elif command -v mariadb-install-db >/dev/null 2>&1; then
        mariadb-install-db "${MYSQLD_USER_ARG[@]}" --datadir="${MYSQL_DATA_DIR}"
    elif command -v mysql_install_db >/dev/null 2>&1; then
        mysql_install_db "${MYSQLD_USER_ARG[@]}" --datadir="${MYSQL_DATA_DIR}"
    else
        echo "No initialization command found for mysql data directory." >&2
        exit 1
    fi
fi

if [[ -S "${MYSQL_SOCKET}" ]]; then
    if mysqladmin --protocol=socket --socket="${MYSQL_SOCKET}" ping >/dev/null 2>&1; then
        echo "mysql already running at ${MYSQL_SOCKET}"
    else
        rm -f "${MYSQL_SOCKET}"
    fi
fi

if ! mysqladmin --protocol=socket --socket="${MYSQL_SOCKET}" ping >/dev/null 2>&1; then
    "${MYSQLD_BIN}" \
        --no-defaults \
        --datadir="${MYSQL_DATA_DIR}" \
        --socket="${MYSQL_SOCKET}" \
        --port="${MYSQL_PORT}" \
        --bind-address=127.0.0.1 \
        --pid-file="${MYSQL_PID_FILE}" \
        --log-error="${MYSQL_LOG_FILE}" \
        --skip-grant-tables \
        --skip-name-resolve \
        "${MYSQLD_USER_ARG[@]}" \
        >"${MYSQL_LOG_FILE}" 2>&1 &
    MYSQLD_PID=$!
    echo "${MYSQLD_PID}" >"${MYSQL_PID_FILE}"
    STARTED_BY_SCRIPT="true"

    for _ in $(seq 1 60); do
        if mysqladmin --protocol=socket --socket="${MYSQL_SOCKET}" ping >/dev/null 2>&1; then
            break
        fi
        sleep 1
    done

    if ! mysqladmin --protocol=socket --socket="${MYSQL_SOCKET}" ping >/dev/null 2>&1; then
        echo "MySQL failed to start. Showing log:" >&2
        tail -n 50 "${MYSQL_LOG_FILE}" >&2 || true
        exit 1
    fi
fi

mysql --protocol=socket --socket="${MYSQL_SOCKET}" --user=root \
    -e "CREATE DATABASE IF NOT EXISTS \`${MYSQL_DATABASE}\`; CREATE DATABASE IF NOT EXISTS \`test\`;" >/dev/null

MYSQL_HOST="127.0.0.1"
MYSQL_URL="mysql://root@${MYSQL_HOST}:${MYSQL_PORT}/${MYSQL_DATABASE}?socket=${MYSQL_SOCKET}"
echo "MySQL ready at ${MYSQL_URL}"
echo "Socket: ${MYSQL_SOCKET}"
echo "Data: ${MYSQL_DATA_DIR}"

if [[ "${RUN_COMMAND}" == "true" ]]; then
    "${CMD[@]}"
    status=$?
    if [[ "${STARTED_BY_SCRIPT}" == "true" ]] && [[ -f "${MYSQL_PID_FILE}" ]]; then
        kill "$(cat "${MYSQL_PID_FILE}")" 2>/dev/null || true
        rm -f "${MYSQL_PID_FILE}"
    fi
    exit ${status}
fi

echo "Server is running in background (PID file: ${MYSQL_PID_FILE})."
if [[ "${STARTED_BY_SCRIPT}" == "true" ]]; then
    echo "Use mysql-test-server stop when finished."
    wait
else
    echo "Server already running; exiting."
fi

#!/usr/bin/env python3
"""Entry point for MariaDB MTR discovery."""

from mariadb_mtr_discover_core import main, parser


if __name__ == "__main__":
    try:
        raise SystemExit(main(parser().parse_args()))
    except (FileNotFoundError, ValueError) as error:
        print(f"MariaDB MTR discovery: {error}", file=__import__("sys").stderr)
        raise SystemExit(2)

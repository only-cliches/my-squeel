#!/usr/bin/env python3
"""MariaDB-named entry point for the shared MTR discovery tool."""

from mysql_mtr_discover import main, parser


if __name__ == "__main__":
    try:
        raise SystemExit(main(parser().parse_args()))
    except (FileNotFoundError, ValueError) as error:
        print(f"MariaDB MTR discovery: {error}", file=__import__("sys").stderr)
        raise SystemExit(2)

#!/usr/bin/env python3
"""MariaDB-named entry point for the shared MTR compatibility runner."""

from mysql_mtr_compat import parser, run


if __name__ == "__main__":
    try:
        raise SystemExit(run(parser().parse_args()))
    except (FileNotFoundError, RuntimeError, ValueError) as error:
        print(f"MariaDB MTR compatibility runner: {error}", file=__import__("sys").stderr)
        raise SystemExit(2)

#!/usr/bin/env python3
"""req_lint: requirement registry linter (Phase 0, unit 1.00).

See scripts/req_lint_lib/cli.py for mode documentation.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from req_lint_lib.cli import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())

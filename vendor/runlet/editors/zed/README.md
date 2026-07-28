# Runlet for Zed

This extension provides `.rnlt` file detection, Tree-sitter syntax
highlighting, bracket matching, comment commands, and indentation.

To install it from this checkout, open Zed's command palette, run
`zed: install dev extension`, and select the `editors/zed` directory.

The extension pins its grammar to the `tree-sitter` branch of
`danielkov/runlet`, where the generated parser is published with the grammar at
the repository root as required by Zed.

# Runlet for Visual Studio Code

This extension provides syntax highlighting, bracket matching, comment
commands, automatic closing pairs, and folding for `.rnlt` files. It has no
runtime dependencies.

To build an installable extension from the repository root:

```sh
(cd editors/vscode && npx --yes @vscode/vsce package \
  --out ../../runlet-language.vsix --allow-missing-repository --skip-license)
code --install-extension runlet-language.vsix
```

For extension development, open `editors/vscode` in VS Code and press F5.

# Tree-sitter grammar for Runlet

`grammar.js` is the source of truth for the Runlet grammar. The files under
`src/` are produced by `tree-sitter generate`; they are intentionally not
committed to the `main` branch.

The generated files are committed only to the repository's `tree-sitter`
branch. That branch contains this directory at its root so consumers such as
Zed can compile `src/parser.c` without running the Tree-sitter generator. Its
`.gitattributes` marks `src/` as generated code on GitHub.

To develop and test the grammar locally:

```sh
npm run generate
npm test
```

These commands recreate the ignored `src/` directory in the working tree.

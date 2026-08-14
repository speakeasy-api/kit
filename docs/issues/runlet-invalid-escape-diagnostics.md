# Runlet invalid string escapes produce cascading diagnostics

A compose script containing a host-language-style escape that Runlet does not accept (for example, `\b` inside a regex passed through a string) correctly reports the invalid escape, but then also reports unrelated invalid characters and an unterminated string later on the same line.

This makes the actionable error harder to spot and suggests that several independent fixes are needed. After reporting an invalid escape, the parser should recover at the closing quote so subsequent diagnostics are not artifacts of the first error.

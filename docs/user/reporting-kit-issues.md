# Reporting Kit Issues

Report Kit bugs and feature requests in the [Kit issue tracker](https://github.com/speakeasy-api/kit/issues). Search open and closed issues before creating a new one so that related reports stay together.

## Ask before an agent submits an issue

An agent must not submit an issue on a user's behalf unless the user explicitly requests it. If an agent finds a potential Kit issue without such a request, it must ask the user before opening the issue. A request to investigate or fix a problem is not permission to publish a report.

## Collect a useful report

Run the following command in the same environment where the problem occurs:

```sh
kit --version
```

Include the complete version output in the issue. If the command does not run, state how Kit was installed or built and include the error instead. Also include:

- a concise description of the problem;
- the expected and actual behavior;
- the smallest reproducible sequence of commands or prompts;
- the operating system and CPU architecture;
- the Kit command, client, editor, or ACP host involved; and
- relevant logs or error messages.

Remove API keys, access tokens, credentials, private prompts, and other sensitive information from commands, configuration, and logs before sharing them. Prefer a minimal configuration that still reproduces the problem.

## Open the issue

Create a [new Kit issue](https://github.com/speakeasy-api/kit/issues/new) with a specific title. Put the version report and reproduction steps in the issue body, use fenced code blocks for commands and logs, and link any related issues or pull requests. After submission, share the issue URL with the user.

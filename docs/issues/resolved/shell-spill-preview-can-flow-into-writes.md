# Shell spill previews could flow into downstream writes

Resolved in Kit 0.1.79.

A `shell` call previously replaced `stdout` or `stderr` larger than 8 KiB with a bounded head/marker/tail preview before returning to Runlet. A dataflow such as:

```text
converted = shell({ command: "render-large-document" })
created = tool({ name: "remote-create", args: { content: converted.stdout } })
return created
```

therefore sent the spill preview to the remote write. The destination received literal `...[shell output spilled: N bytes; see artifact field]...` text with the document's middle omitted, even though the model did not receive or retransmit the value between those calls. This caused an Obsidian-to-Notion sync on Kit 0.1.75 to upload truncated 48,081-byte and 25,229-byte converted Markdown documents.

Shell stdout and stderr now remain complete for internal Runlet consumers. The spill guard runs on the final compose result instead, immediately before it enters model context. Oversized final results are stored as complete artifacts and represented to the model by a bounded preview, artifact path, and original byte count. Separate 64 MiB safety limits fail rather than substituting partial internal content. Focused tests cover passing complete oversized shell output into a downstream write and spilling only the final result.

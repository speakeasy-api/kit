# GitHub tarballs fail on the PAX global header

Kit rejected a checksum-pinned GitHub commit `tar.gz` archive before plugin validation with:

```text
Error: "unsupported tar entry type at pax_global_header"
```

GitHub-generated tarballs can contain the standard PAX global extended-header entry. The plugin archive loader now accepts this bounded metadata entry without extracting it, while it continues to reject links and other unsupported tar payload types.

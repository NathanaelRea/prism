# Development

Run the local CI gate before pushing:

```sh
scripts/full-check.sh
```

To enforce the same gate as a pre-push hook, opt into the versioned hooks:

```sh
git config core.hooksPath .githooks
```

# Create and Publish Prism Packages

Packages are editable working copies backed by immutable retained revisions.

1. Create a package with `prism package new <qualified.package> [directory]`.
2. Give every exported resource a qualified identity in the package namespace and include its
   exact SHA-256 digest in `prism-package.toml`.
3. Run `prism package validate <directory>` and validate/preview every exported Workflow
   Definition. Check and test every extension executable for each advertised target.
4. Install a local release candidate with `prism package install <directory>` and inspect it with
   `prism package show <id> --json`.
5. Publish immutable archives and hashes. Updates must use a non-destructive three-way merge; do
   not edit `package.lock` or retained store bytes by hand.

Repository-local executable resources require explicit trust. Never silently shadow a global
resource identity or overwrite a customized working copy.

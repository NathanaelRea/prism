-- Legacy workflow tables are removed transactionally during pre-SQLx adoption. SQLx-owned
-- databases never contained them, so this migration only fences out pre-SQLx Prism binaries.
pragma user_version = 2147483647;

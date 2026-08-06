select count(*) as "count!: i64" from pending_worktree_deletion
where branch = ? and worktree_removed = 0
